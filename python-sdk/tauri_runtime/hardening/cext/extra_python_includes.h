// 常量块 / 模块名加密的声明与宏覆盖；函数体在 nuitka_hardening_decrypt.c，构建那一半和完整说明
// 都在 tauri_runtime/hardening/plugin.py。
//
// 本文件存为带 BOM 的 UTF-8：它会被 include 进三个平台上每一个 C 文件。中文注释靠 BOM 让
// MSVC（中文 locale 的 Windows 下）按 UTF-8 正确读，避免 C4819 吞掉下一行；clang / gcc 能
// 跳过被 include 文件开头的 BOM。
//
// 文件名 extra_python_includes.h 是 Nuitka 的 prelude.h 写死 #include 的，不能改。设了
// _NUITKA_EXPERIMENTAL_EXTRA_INCLUDES 后，prelude.h 会在末尾 include 本文件；设
// _NUITKA_EXPERIMENTAL_WRITEABLE_CONSTANTS 只是让 prelude.h 不再自带空的 DECODE / UN_TRANSLATE，
// 把这两个宏留给我们覆盖（我们并不往常量块里写，解密是解到堆上）。Nuitka 生成的代码随后在常量块上
// 调用我们的 DECODE()、在每个编成 C 的模块的常量块名字上调用 UN_TRANSLATE()，加载器则通过每个
// 表项的 "m_get_name" 调我们的解码函数。

#ifndef TAURI_RUNTIME_HARDENING_INCLUDES_H
#define TAURI_RUNTIME_HARDENING_INCLUDES_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// 把常量块解密到堆上，再把 blob 指针指过去。
void Nuitka_Hardening_DecodeConstants(unsigned char const **blob);

// 加载器的 "m_get_name"：把加密的模块名解到调用方给的缓冲区里。
void Nuitka_Hardening_DecodeModuleName(char *buffer, size_t buffer_size, char const *name);

// 解开某个编成 C 的模块传给 loadConstantsBlob() 的名字。
char const *Nuitka_Hardening_DecodeBlobName(char const *name);

#ifdef __cplusplus
}
#endif

#define DECODE(x) Nuitka_Hardening_DecodeConstants((unsigned char const **)&(x))
#define UN_TRANSLATE(x) Nuitka_Hardening_DecodeBlobName(x)

#endif
