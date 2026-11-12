"""加固靶子的自有模块之一：承载密语常量与若干 f-string。

打包后用来验证：常量块被加密（产物里查不到 PROMPT 明文、模块名 secret_pkg.analysis 也查不到），
且运行时能把常量与 f-string 的静态段都正确解回原值（见 main.py 的自检）。
"""

PROMPT = "SPARROW-PROMPT-6f1c2a-DO-NOT-LEAK"


def greeting(name: str) -> str:
    # f-string 的静态段 " says SPARROW-..." 同样是常量，必须被加密、运行时再解回。
    return f"{name} says {PROMPT}"


def render() -> str:
    # 前后都有静态段的 f-string：验证多段常量都被正确处理。
    return f"[header] {PROMPT} [footer]"


def strip_marker_9c7d41e2() -> int:
    """符号表 strip 的探针：名字刻意独特。编成 C 后它会成为一个本地符号，strip 应把它删掉，
    因此 verify_hardening.py 扫描产物时这个名字不应以明文出现。被 main 调用以免被优化消除。"""
    return len(PROMPT)
