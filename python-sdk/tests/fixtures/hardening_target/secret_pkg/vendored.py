"""加固靶子的第三方依赖探针：验证第三方库被打成字节码后，运行时元数据查询仍可用。

对应加固插件的两段逻辑：`decideCompilation` 把 site-packages 里的包判成字节码，以及
`_third_party` 里的 `addDistributionMetadataValue` 手动登记包元数据——字节码里 Nuitka 看不出
`importlib.metadata.version(...)` 调用，不登记就会在运行时抛 PackageNotFoundError。
"""

import importlib.metadata

import typing_extensions  # 第三方包：会被加固插件编成字节码而非 C


def third_party_version() -> str:
    # 故意用下划线写法查询，而发行名是 typing-extensions（连字符）。Nuitka 运行时按名查元数据
    # 只忽略大小写、不把 - 和 _ 视为相同，必须依赖插件同时登记了两种别名，否则这里抛错。
    return importlib.metadata.version("typing_extensions")


def third_party_works() -> bool:
    # 真正用一下 typing_extensions，确认字节码模块能正常导入执行，而不只是元数据可查。
    return typing_extensions.get_origin(list[int]) is list
