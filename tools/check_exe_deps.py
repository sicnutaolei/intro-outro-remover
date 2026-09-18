"""直接解析 PE 文件的导入表，列出运行期真正需要的 DLL。

为什么不用 grep：可执行文件里到处是依赖库留下的字符串，
grep "xxx.dll" 会命中一堆根本没被导入的名字。要判断「这个 exe 在干净机器上
能不能跑」，只能看导入表 —— 那是加载器真正会去解析的清单。

判定标准只有一条：输出里如果出现 VCRUNTIME140.dll / MSVCP140.dll 之类，
说明 C 运行库是**动态**链接的，这个 exe 拿到没装 VC++ 可再发行组件包的机器上会起不来。

用法（只用标准库，不需要装任何第三方包）：

    python tools/check_exe_deps.py target/release/intro-outro-gui.exe
"""

import struct
import sys
from pathlib import Path


def rva_to_offset(sections, rva):
    for va, vsize, raw_size, raw_ptr in sections:
        if va <= rva < va + max(vsize, raw_size):
            return raw_ptr + (rva - va)
    return None


def parse(path):
    data = Path(path).read_bytes()

    if data[:2] != b"MZ":
        sys.exit("不是 PE 文件：缺少 MZ 头")
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    if data[e_lfanew : e_lfanew + 4] != b"PE\0\0":
        sys.exit("不是 PE 文件：缺少 PE 签名")

    coff = e_lfanew + 4
    machine, n_sections = struct.unpack_from("<HH", data, coff)
    size_opt = struct.unpack_from("<H", data, coff + 16)[0]
    opt = coff + 20
    magic = struct.unpack_from("<H", data, opt)[0]

    # PE32+ 的数据目录从可选头偏移 112 开始，PE32 从 96 开始
    if magic == 0x20B:
        kind, dd_off = "PE32+ (64 位)", 112
    elif magic == 0x10B:
        kind, dd_off = "PE32 (32 位)", 96
    else:
        sys.exit(f"未知的可选头 magic：0x{magic:x}")

    # 数据目录第 1 项 = 导入表
    import_rva, import_size = struct.unpack_from("<II", data, opt + dd_off + 8)

    sec_off = opt + size_opt
    sections = []
    for i in range(n_sections):
        base = sec_off + i * 40
        vsize, va, raw_size, raw_ptr = struct.unpack_from("<IIII", data, base + 8)
        sections.append((va, vsize, raw_size, raw_ptr))

    dlls = []
    if import_rva:
        off = rva_to_offset(sections, import_rva)
        while True:
            desc = struct.unpack_from("<IIIII", data, off)
            if not any(desc):
                break
            name_rva = desc[3]
            n_off = rva_to_offset(sections, name_rva)
            end = data.index(b"\0", n_off)
            dlls.append(data[n_off:end].decode("ascii"))
            off += 20

    return kind, machine, dlls


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    path = sys.argv[1]
    kind, machine, dlls = parse(path)

    arch = {0x8664: "x86-64", 0x14C: "x86", 0xAA64: "ARM64"}.get(machine, f"0x{machine:x}")

    # Windows 10/11 自带的运行时：UCRT（ucrtbase 与 api-ms-win-crt-*）是系统组件。
    # VCRUNTIME140 / MSVCP140 属于 VC++ 可再发行组件包，**不是**系统自带。
    vc_redist = [d for d in dlls if d.lower().startswith(("vcruntime", "msvcp", "concrt"))]

    print(f"文件      : {path}")
    print(f"格式      : {kind} / {arch}")
    print(f"导入 DLL  : {len(dlls)} 个")
    for d in sorted(dlls):
        print(f"  - {d}")

    print()
    if vc_redist:
        print("❌ 需要 VC++ 可再发行组件包（干净 Windows 上会报「找不到 xxx.dll」）：")
        for d in vc_redist:
            print(f"     {d}")
        sys.exit(1)
    print("✅ 导入表里没有任何 VC++ 运行库 —— 干净 Windows 上可直接运行")


if __name__ == "__main__":
    main()
