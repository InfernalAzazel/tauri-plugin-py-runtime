// 加固插件的运行时解密实现，通过 getExtraCodeFiles 作为一个额外的 C 文件交给 Nuitka
// （和官方 dill-compat 插件用 DillPlugin.c 的方式一样）。声明和配套的宏在头文件里，头文件
// 经 prelude.h 被 include 进每一个翻译单元；本文件作为一个普通编译单元提供函数定义，像其它
// Nuitka C 文件一样参与链接。
//
// 三个函数都用和 Python 端（tauri_runtime/hardening/plugin.py）相同的 LCG 密钥流异或。
// 本文件存为带 BOM 的 UTF-8（原因见头文件）。
//
// 刻意写成 C89 风格（变量全在块首声明、循环变量不在 for 里声明），这样无论被 MSVC 当成 C89、
// C11 还是在 Nuitka 的 non-C11 回退里当成 C++ 编译都能过。

#include "extra_python_includes.h"

#include <stdint.h>
#include <stdlib.h>

#ifndef NUITKA_HARDENING_KEY
// 兜底：仅在脱离插件单独编译时用（比如 C 自测）。真实构建时插件会在文件最前面注入
// 每次构建都确定的密钥，覆盖掉这里。
#define NUITKA_HARDENING_KEY 0xA5C3F17Bu
#endif

// 构建时插件把常量块换成了：4 字节小端长度，后面是与密钥流异或过的块。我们解密到堆内存再把
// 指针指过去，这样无论平台用哪种方式嵌入常量块（只读段、COFF 对象、incbin、#embed）都能用。
void Nuitka_Hardening_DecodeConstants(unsigned char const **blob) {
    unsigned char const *src = *blob;
    uint32_t len;
    unsigned char *out;
    volatile uint32_t key;
    uint32_t k;
    uint32_t i;

    len = (uint32_t)src[0] | ((uint32_t)src[1] << 8) | ((uint32_t)src[2] << 16) | ((uint32_t)src[3] << 24);
    src += 4;

    // 常量活到进程结束，所以这块永不释放。
    out = (unsigned char *)malloc(len);
    if (out == NULL) {
        abort();
    }

    // 必须 volatile，否则优化器会在编译期把密文解开、再把明文塞回二进制（某些嵌入方式会把
    // 常量块变成优化器能直接看到的 C 数组）。
    key = NUITKA_HARDENING_KEY;
    k = key;
    for (i = 0; i < len; i++) {
        k = k * 1664525u + 1013904223u;
        out[i] = src[i] ^ (unsigned char)(k >> 24);
    }

    *blob = out;
}

// 模块名是常量块之外的 C 字符串，出现在两处：加载表项，以及每个编成 C 的模块传给
// loadConstantsBlob() 的名字。插件把两处都存成：1 字节盐，后面是（长度字节 + 名字）与由
// 密钥和盐一起起头的密钥流异或的结果。这个函数就是加载器的 "m_get_name"。
void Nuitka_Hardening_DecodeModuleName(char *buffer, size_t buffer_size, char const *name) {
    unsigned char const *src = (unsigned char const *)name;
    volatile uint32_t key;
    uint32_t k;
    size_t len;
    size_t i;

    key = NUITKA_HARDENING_KEY;
    k = key ^ ((uint32_t)src[0] * 0x9E3779B1u);
    k = k * 1664525u + 1013904223u;
    len = (size_t)(src[1] ^ (unsigned char)(k >> 24));
    if (len >= buffer_size) {
        len = buffer_size - 1;
    }

    for (i = 0; i < len; i++) {
        k = k * 1664525u + 1013904223u;
        buffer[i] = (char)(src[i + 2] ^ (unsigned char)(k >> 24));
    }
    buffer[len] = 0;
}

// 供 loadConstantsBlob() 用的名字。每个编成 C 的模块只调一次，所以缓冲区直接不释放
// （长度字节把名字上限卡在 255）。
char const *Nuitka_Hardening_DecodeBlobName(char const *name) {
    char *buffer = (char *)malloc(256);
    if (buffer == NULL) {
        abort();
    }

    Nuitka_Hardening_DecodeModuleName(buffer, 256, name);
    return buffer;
}
