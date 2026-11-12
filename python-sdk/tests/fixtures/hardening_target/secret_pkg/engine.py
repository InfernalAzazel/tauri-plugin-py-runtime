"""加固靶子的自有模块之二：依赖 analysis。

用来验证两件事：多个自有模块的模块名（secret_pkg.engine 也在内）都被加密；
以及加固后跨模块 import、调用和基于解密常量的计算仍然正常。
"""

from secret_pkg.analysis import PROMPT, greeting, render


def summary(name: str) -> dict[str, int]:
    """返回几个基于解密常量计算出的长度，供 main.py 与预期值比对。"""
    return {
        "prompt_len": len(PROMPT),
        "greeting_len": len(greeting(name)),
        "render_len": len(render()),
    }
