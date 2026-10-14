#!/usr/bin/env python3
"""把 Android release keystore 写进 godot-project/export_presets.cfg。

在 CI（或本地）导出 Android release 前运行。读取下列环境变量：
  ANDROID_KEYSTORE_PATH       keystore 路径（导出直接在 runner 上跑 Godot，
                              写 runner 上可见的绝对路径，例如
                              /home/runner/work/<repo>/<repo>/.android-keystore/release.keystore）
  ANDROID_KEYSTORE_ALIAS      key alias（Godot 的 keystore/release_user）
  ANDROID_KEYSTORE_PASSWORD   store / key 密码
可选：
  ANDROID_PRESET_NAME         目标预设名，默认 "Android arm64"
  GODOT_EXPORT_PRESETS        预设文件路径，默认 godot-project/export_presets.cfg
"""
import os
import pathlib
import re
import sys

CFG = pathlib.Path(os.environ.get("GODOT_EXPORT_PRESETS", "godot-project/export_presets.cfg"))
preset_name = os.environ.get("ANDROID_PRESET_NAME", "Android arm64")
ks_path = os.environ["ANDROID_KEYSTORE_PATH"]
alias = os.environ["ANDROID_KEYSTORE_ALIAS"]
password = os.environ["ANDROID_KEYSTORE_PASSWORD"]


def esc(value: str) -> str:
    """转义 Godot ConfigFile 字符串里的反斜杠与双引号。"""
    return value.replace("\\", "\\\\").replace('"', '\\"')


lines = CFG.read_text(encoding="utf-8").splitlines()

# 先按 name 找到 Android 预设的编号（不依赖固定的 preset.N 顺序）。
target = None
cur = None
for line in lines:
    m = re.match(r"^\[preset\.(\d+)\]$", line)
    if m:
        cur = int(m.group(1))
        continue
    m = re.match(r'^name="(.*)"$', line)
    if m and cur is not None and m.group(1) == preset_name:
        target = cur
        break

if target is None:
    sys.exit(f"找不到 Android 预设: {preset_name}")

header = f"[preset.{target}.options]"
out = []
inserted = False
for line in lines:
    if line.strip() == header:
        out.append(line)
        out.append(f'keystore/release="{esc(ks_path)}"')
        out.append(f'keystore/release_user="{esc(alias)}"')
        out.append(f'keystore/release_password="{esc(password)}"')
        inserted = True
        continue
    # 去掉旧的 release keystore 行，避免重复键。
    if re.match(r"^keystore/release(_user|_password)?\s*=", line):
        continue
    out.append(line)

if not inserted:
    sys.exit(f"找不到预设选项段: {header}")

CFG.write_text("\n".join(out) + "\n", encoding="utf-8")
print(f"已为预设 {preset_name} 配置 release keystore: {ks_path} (alias={alias})")
