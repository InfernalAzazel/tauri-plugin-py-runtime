"""Nuitka 打包加固插件，四件事：

1. 第三方库以字节码打包、使用方自己的代码编成 C——库编成 C 几乎换不来运行速度（瓶颈在网络、浏览器、模型），
   却占了绝大部分编译时间。
2. 常量块加密——编成 C 后字符串常量（含业务提示词）仍原样躺在二进制里，`strings` 就能读。这里在打包时把
   常量块加密、运行时解密，对应商业版的 constant value encryption（见 cext/）。
3. 模块名加密——使用方的模块名（`app.ai.analysis` 这类）不在常量块里，而在 Nuitka 的模块加载表里，同样加密。
4. strip 去掉符号表里的函数名，别人就看不出有哪些函数。

2、3 只是让 `strings` 之类的直接搜索落空：密钥和算法都在二进制里，运行起来后内存里也都是明文。
macOS、Linux、Windows 三个平台共用这一份：平台差异只有第 4 条（见 onStandaloneBinary）。

「哪些算使用方自己的源码」按项目根判断：优先读环境变量 TAURI_RUNTIME_PROJECT_ROOT，没设就用当前工作目录
（`tauri-runtime build` 构建时会把工作目录设成项目根）。
"""

import importlib.util
import os
import secrets
import struct
import subprocess
import sys
import zlib
from functools import cache
from pathlib import Path
from typing import Any

from nuitka.code_generation.ConstantCodes import addDistributionMetadataValue
from nuitka.plugins.PluginBase import NuitkaPluginBase
from nuitka.utils.Distributions import (
    getDistributionName,
    getDistributionsFromModuleName,
)
from nuitka.utils.ModuleNames import trigger_names

_PROJECT = Path(os.environ.get("TAURI_RUNTIME_PROJECT_ROOT") or Path.cwd()).resolve()
_CEXT = Path(__file__).resolve().parent / "cext"

# ── 与 C 侧（cext/）的约定：下面这些名字两端必须一致，也是抽象通用插件时要参数化的接缝 ──
_C_SOURCE = _CEXT / "nuitka_hardening_decrypt.c"  # 运行时解密实现，交给 getExtraCodeFiles 编译
_KEY_MACRO = "NUITKA_HARDENING_KEY"  # C 侧读密钥用的宏名，定义由 getExtraCodeFiles 注入
_DECODE_NAME_FUNC = "Nuitka_Hardening_DecodeModuleName"  # 加载表项 m_get_name 指向的模块名解码函数
_CONST_BLOB = "__constant.bin"  # 待加密的常量块文件名（此刻还在磁盘上）
_KEY_ENV = "TAURI_RUNTIME_HARDENING_KEY"  # 固定密钥用的环境变量名（供测试/复现构建）
_METADATA_TAG = "tauri-runtime-hardening"  # 登记字节码库元数据时的归类标签

# 解密密钥。优先读环境变量（测试时传 TAURI_RUNTIME_HARDENING_KEY=0xA5C3F17B 固定下来，生成的 C 不变、
# ccache 能命中、构建快）；没设就每次构建随机生成——常量块和模块名两处加密都用它，C 端的宏由
# getExtraCodeFiles 注入同一个值。随机密钥的代价是密文每次都变、ccache 全失效、要整包重编（约 7 分钟）。
_key_env = os.environ.get(_KEY_ENV)
_KEY = int(_key_env, 0) & 0xFFFFFFFF if _key_env else secrets.randbits(32)

# LCG 密钥流的参数，必须和 nuitka_hardening_decrypt.c 完全一致。
_LCG_MUL = 1664525
_LCG_ADD = 1013904223
_SALT_MUL = 0x9E3779B1


def _crypt(data: bytes, seed: int = _KEY) -> bytes:
    """LCG 密钥流异或（对称，加解密同一套）；与 nuitka_hardening_decrypt.c 的实现一致。"""
    out = bytearray(len(data))
    k = seed
    for i, byte in enumerate(data):
        k = (k * _LCG_MUL + _LCG_ADD) & 0xFFFFFFFF
        out[i] = byte ^ ((k >> 24) & 0xFF)
    return bytes(out)


def _crypt_name(name: bytes) -> bytes:
    """模块名密文：1 字节盐 + 加密后的（长度字节 + 名字），由 Nuitka_Hardening_DecodeModuleName 解开。"""
    assert len(name) < 256, name
    # 每个名字用自己的盐起一条密钥流，否则相同的前缀会加密成相同的密文。盐取自名字本身，
    # 保证同一个名字每次构建都加密成同样的结果（ccache 才能命中）。
    salt = zlib.crc32(name) & 0xFF
    return bytes([salt]) + _crypt(bytes([len(name)]) + name, _KEY ^ (salt * _SALT_MUL & 0xFFFFFFFF))


@cache
def _third_party(package: str) -> bool:
    try:
        spec = importlib.util.find_spec(package)
    except (ImportError, ValueError):
        return False
    if spec is None:
        return False
    origin = spec.origin or next(iter(spec.submodule_search_locations or ()), None) or ""
    if "site-packages" not in origin:
        return False
    # 编成 C 时 Nuitka 能看出 `importlib.metadata.version(...)` 之类的调用并带上包的元数据，字节码里看不出，
    # 不手动带上，运行时就报 PackageNotFoundError。Nuitka 运行时按名字查元数据只忽略大小写，
    # 不把 `-` 和 `_` 视为相同（`version("genai_prices")` 查不到 `genai-prices`），两种写法都登记。
    for distribution in getDistributionsFromModuleName(package):
        name = getDistributionName(distribution)
        if name is None:
            continue
        for alias in {name, name.replace("-", "_")}:
            addDistributionMetadataValue(alias, distribution, _METADATA_TAG)
    return True


def _is_own_source(module) -> bool:
    """模块是不是使用方自己项目里的源码（排除主模块和第三方库）。模块名加密只针对这些。"""
    if module.isMainModule():
        return False
    path = Path(module.getCompileTimeFilename()).resolve()
    return path.is_relative_to(_PROJECT) and "site-packages" not in path.parts


class NuitkaPluginHardening(NuitkaPluginBase):
    plugin_name = "tauri-runtime-hardening"
    plugin_desc = "Harden the Nuitka build: third-party packages as bytecode, encrypted constants and module names, stripped symbols."
    plugin_category = "obfuscation"

    def decideCompilation(self, module_name) -> Any:
        # 插件生成的 `xxx-preLoad` 等触发模块只有几行；判成字节码再碰上字节码缓存，Nuitka 自己会崩
        if str(module_name).endswith(trigger_names):
            return None
        return "bytecode" if _third_party(str(module_name.getTopLevelPackageName())) else None

    def getPreprocessorSymbols(self, onefile) -> Any:
        # WRITEABLE_CONSTANTS：让 prelude.h 不再自带空的 DECODE/UN_TRANSLATE（我们并不往常量块里写，解密是解到堆上）；
        # EXTRA_INCLUDES：让 prelude.h 末尾 #include 我们的 extra_python_includes.h 来补这两个宏。
        # 这两个宏是 Nuitka 生成代码里直接调用的，只能靠这个 include 口子覆盖；getExtraCodeFiles 加的是独立
        # 编译单元，改不了别人的宏。所以常量块加密这块离不开这两个实验开关。
        return {"_NUITKA_EXPERIMENTAL_WRITEABLE_CONSTANTS": "1", "_NUITKA_EXPERIMENTAL_EXTRA_INCLUDES": "1"}

    def getExtraIncludeDirectories(self) -> Any:
        # 让上面的 include 以及 getExtraCodeFiles 里的 .c 都能找到 extra_python_includes.h。
        return [str(_CEXT)]

    def getExtraCodeFiles(self) -> Any:
        # 和官方 dill-compat 插件（DillPlugin.py + DillPlugin/DillPlugin.c）一样：解密函数的实现作为一个独立
        # .c 交给 Nuitka 编译，头文件只留宏和声明。函数在这里定义、在生成代码里被调用，靠链接连起来。
        source = _C_SOURCE.read_text(encoding="utf-8-sig")
        # 在最前面注入本次构建的密钥（覆盖掉 .c 里的 #ifndef 兜底，两端始终一致）。返回 bytes：Nuitka 对
        # bytes 走 putBinaryFileContents 原样写入，不走 latin1 的 writeSourceCode（中文注释会写崩），开头
        # 的 UTF-8 BOM 也让 Windows 的 MSVC 按 UTF-8 正确读中文。
        source = f"#define {_KEY_MACRO} 0x{_KEY:08X}u\n" + source
        return {_C_SOURCE.name: b"\xef\xbb\xbf" + source.encode("utf-8")}

    def onDataComposerResult(self, blob_filenames) -> None:
        # 常量块此刻还是磁盘上的 .bin 文件、尚未嵌入二进制：加密它，并在最前面塞 4 字节小端长度（供运行时解密用）。
        for blob_filename in blob_filenames:
            if os.path.basename(blob_filename) != _CONST_BLOB:
                continue
            data = Path(blob_filename).read_bytes()
            Path(blob_filename).write_bytes(struct.pack("<I", len(data)) + _crypt(data))

    def onMetaPathLoaderEntryTemplate(self, module, template_args) -> None:
        # 模块名不在常量块里，而是以 C 字符串躺在二进制里，`strings` 能直接列出模块结构。一共两处，
        # 这里是第一处：Nuitka 的模块加载表。只处理使用方自己的模块——第三方库的名字本来就是公开的，
        # 全部加密还会让每次 import 多解四千多个名字。
        if not _is_own_source(module):
            return
        if template_args["get_name_func"] != "NULL":
            return
        literal = "".join(f"\\x{byte:02x}" for byte in _crypt_name(module.getFullName().asBytes()))
        template_args["module_name"] = f'"{literal}"'
        template_args["get_name_func"] = _DECODE_NAME_FUNC

    def encodeDataComposerName(self, data_name) -> Any:
        # 第二处：每个编成 C 的模块用自己的名字到常量块里找自己的常量。生成的代码是
        # `loadConstantsBlob(..., UN_TRANSLATE("名字"))`，这里换成密文，头文件里的 UN_TRANSLATE 负责解回来。
        # 钩子只给名字、不给模块，所以编成 C 的模块（使用方代码和几个触发模块）一律加密。
        return _crypt_name(data_name)

    def onStandaloneBinary(self, filename) -> None:
        # 本地函数符号这关，Linux / Windows 上 Nuitka 默认就删干净了：Linux 给链接器加 `-s`（删掉整个符号表），
        # Windows 加 `/DEBUG:NONE`（函数名不进 PE）。唯独 macOS 被 Nuitka 的 `not isMacOS()` 守卫故意跳过
        # （strip 会让签名失效、影响公证），所以只有这里需要补，别的平台直接返回。
        if sys.platform != "darwin":
            return
        # 生成之后、Tauri 签名之前，去掉符号表（`strip -x` 只删本地符号，不动导出符号，不影响运行）。
        subprocess.run(["strip", "-x", filename], check=True)
        # strip 会让签名失效；macOS 单独启动这个嵌套后端时会校验它自身的签名，这里补一个 ad-hoc 签名。
        # 外层 app 的正式签名由 Tauri 之后统一处理，会把 strip 后的字节一起封存。
        subprocess.run(["codesign", "--force", "--sign", "-", filename], check=True)
