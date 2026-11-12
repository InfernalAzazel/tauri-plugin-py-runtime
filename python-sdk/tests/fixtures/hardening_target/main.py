"""加固靶子入口：打包后运行它，确认加固没有破坏行为。

自检一律走 `_require`（显式 `raise SystemExit`）而非 `assert`——构建带 `--python-flag=no_asserts`，
`assert` 会被整个删掉，自检就形同虚设。检查覆盖：解密后的常量值、多段 f-string 静态段、
跨模块 import 与基于解密常量的计算、第三方库字节码导入与元数据查询，以及 strip 探针函数可调用。
全部通过才打印 TOKEN，供 verify_hardening.py 判定运行时解密成功。
"""

import importlib.metadata

from secret_pkg.analysis import PROMPT, greeting, render, strip_marker_9c7d41e2
from secret_pkg.engine import summary
from secret_pkg.vendored import third_party_version, third_party_works

EXPECTED_PROMPT = "SPARROW-PROMPT-6f1c2a-DO-NOT-LEAK"
EXPECTED_GREETING = f"x says {EXPECTED_PROMPT}"
EXPECTED_RENDER = f"[header] {EXPECTED_PROMPT} [footer]"
TOKEN = "handshake-ok-42"


def _require(ok: bool, what: str) -> None:
    if not ok:
        raise SystemExit(f"hardening self-check failed: {what}")


def main() -> None:
    # 常量块加密：常量与 f-string 静态段都要解回原值。
    _require(PROMPT == EXPECTED_PROMPT, "constant decryption")
    _require(greeting("x") == EXPECTED_GREETING, "f-string decryption (greeting)")
    _require(render() == EXPECTED_RENDER, "f-string decryption (render)")

    # 模块名加密：跨自有模块 import 与基于解密常量的计算仍正常。
    stats = summary("x")
    _require(stats["prompt_len"] == len(EXPECTED_PROMPT), "cross-module import (engine.summary)")
    _require(stats["greeting_len"] == len(EXPECTED_GREETING), "cross-module computed value (greeting)")
    _require(stats["render_len"] == len(EXPECTED_RENDER), "cross-module computed value (render)")

    # 第三方库打成字节码：模块能导入执行，且元数据（含 -/_ 别名）在运行时可查。
    _require(third_party_works(), "third-party bytecode import (typing_extensions)")
    try:
        version = third_party_version()
    except importlib.metadata.PackageNotFoundError:
        raise SystemExit("hardening self-check failed: third-party metadata missing (addDistributionMetadataValue)")
    _require(bool(version), "third-party metadata version empty")

    # strip 探针：函数本身照常可调用（strip 只删符号名，不影响运行）；它的符号名不该出现在产物里，
    # 由 verify_hardening.py 扫描产物字节确认。
    _require(strip_marker_9c7d41e2() == len(EXPECTED_PROMPT), "strip marker callable")

    print(TOKEN)


if __name__ == "__main__":
    main()
