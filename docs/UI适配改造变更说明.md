---
title: UI 适配改造变更说明
type: 变更说明
status: active
created: 2026-10-22
updated: 2026-10-22
tags:
  - UI
  - 输入
  - 变更说明
source: 本轮 UI 适配改造（P0-P5），基线 commit f77a6b3
---

# UI 适配改造变更说明

一句话：本轮在 Godot 客户端把「设备能力（platform）/ 输入方案（scheme）/ 布局与交互」解耦；动作按钮从触屏控件层里的原生 Button 生成改为「ButtonSpecs 数据映射 + 自绘 ActionButton + ActionButtonHost 宿主」（自绘是可选增强，不是绕开引擎能力）。Rust 侧零改动。

范围：只改 `godot-project/` 与 `docs/`，不动 Rust crate 与 CI。设计口径见 [[10-Godot-UI方案]]。

## 阶段总览

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| P0 | 注册表 remove 路径先 detach 再释放；空 manager 不写 `backend_paused` | 已提交 `9715beb` |
| P1 | `UiState.platform` 三值；缩放 / 边距按 platform，不看 scheme | 已提交 `bd97d7f` |
| P2 | `scheme_overrides` + `HudRegistry.relocate()`：按方案覆盖 tray / align / priority / min_size | 已提交 `0c0d2cb` |
| P3 | `ButtonSpecs` / `ActionButtonHost` / 自绘 `ActionButton`；控件层只剩摇杆 + 视角区 | 已提交 `e0123d9` + 收尾 `7c6f1ac` |
| P3b | 交互提示用真实绑定键位（`OS.get_keycode_string(physical_keycode)`）；触屏显示 `Tap` | 已提交 `e0123d9` |
| P4 | 同 `group_id` 元素互斥，只激活当前 scheme 下 priority 最高者 | 已提交 `73443fc` |
| P5 | 混合设备防抖：scheme 切换冷却、摇杆阈值、鼠标微动忽略 | 已提交 `dc28db3` |
| P6 | 移动端入口：触屏暂停按钮、触摸滑条、debug 构建的暂停菜单 Debug 区 | 已提交 `db7dcae` |

## P0：注册表释放路径与暂停标志

**问题**：`unregister` / `register_external` 拒绝只 `queue_free`，元素在 `_on_attach()` 里订阅的载荷回调与 `controlled_entity_changed` 信号不断开，回调会打到已释放实例；headless / 纯 UI 预览没有 manager 时 `open_pause_menu()` 仍把 `UiState.backend_paused` 置 true。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/hud_widget.gd` | 新增 `detach()` / `_on_detach()` 钩子（幂等），attach / detach 成对 |
| `godot-project/ui/hud_registry.gd` | `unregister` 改走 `_detach_widget`：先 detach 再移出容器；`register_external` 入树前先量一次尺寸，入树后超限走 `unregister` 回滚 |
| `godot-project/ui/ui_root.gd` | `_set_backend_paused` 先取 manager，null 时直接返回，不写 `backend_paused` |
| `godot-project/bevy_client/bevy_client.gd` | 新增 `unlisten_component(kind, callable)` |
| `godot-project/bevy_client/sync/dispatcher.gd` | 新增 `unlisten`；`_notify_listeners` 遍历副本，允许回调里注销 |
| `godot-project/ui/widgets/crosshair.gd` | `_on_detach` 断开 `input_look_changed` |
| `godot-project/ui/widgets/health_bar.gd` | `_on_detach` 取消 HEALTH 订阅 + 断开 `controlled_entity_changed` |
| `godot-project/ui/widgets/interact_prompt.gd` | `_on_detach` 取消 INTERACTION 订阅 + 断开 `controlled_entity_changed` |
| `godot-project/test/ui_smoke_test.gd` | 新增 8 项检查：无 manager 不写暂停标志、超尺寸元素不残留、attach/detach 回调与信号成对 |

**验证**：`tools/ui_check.sh` 通过；checks 由 43 增至 51。

## P1：platform 与 scheme 分离

**问题**：边距与缩放原来按 `UiState.has_scheme(TOUCH)` 判断，平板接键盘、桌面触屏一体机等混合设备上，`scheme` 一抖界面就跳。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/ui_state.gd` | 新增 `Platform { DESKTOP, PHONE, TABLET }` 与静态字段 `platform`；`reset()` 复位 |
| `godot-project/ui/ui_root.gd` | 新增 `_update_platform()`，在 `_apply_scale()` 开头写 platform；`debug_force_mobile` 视为 PHONE；基础边距改 `TOUCH_PAD if platform != DESKTOP else EDGE_PAD` |
| `godot-project/test/ui_smoke_test.gd` | 新增 6 项检查：桌面 platform、切 scheme 不改 platform、边距仍 EDGE_PAD、`debug_force_mobile` → PHONE 且 TOUCH_PAD、关闭后回 DESKTOP |

**验证**：`tools/ui_check.sh` 通过，checks=57。三量分离的口径见 [[10-Godot-UI方案]] 的「platform / scheme / compact_level 三量分离」。

## P2：scheme_overrides 与 relocate

**目标**：同一元素在不同输入方案下换槽位 / 对齐 / 次序 / 尺寸，不需要两套场景，也不需要元素自己判断设备。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/hud_widget.gd` | 新增 `@export var scheme_overrides: Dictionary`；`effective_tray()` / `effective_align()` / `effective_priority()` / `effective_min_size()`；`refresh_scheme()` 应用 `effective_min_size`；覆盖 key 先精确匹配、再按位掩码相交匹配；默认 min_size 首次需要时记下（不依赖 `_ready()`） |
| `godot-project/ui/hud_registry.gd` | 注册与排序改用 `effective_*`；新增 `relocate(widget, current)`：移除后按新落点重插（同槽也重排）；`refresh_scheme()` 先 relocate 再广播；`register_external` 接受 `scheme_overrides`，尺寸上限改为对 `all_min_sizes()` 每个方案都检查 |
| `godot-project/test/ui_smoke_test.gd` | 新增 15 项检查：`relocate` 落点 / 对齐 / 优先级重排、`min_size` 覆盖与恢复、mod meta 的 `scheme_overrides`、每方案尺寸上限等 |

**验证**：`tools/ui_check.sh` 通过，checks 由 57 增至 72。**未单测**：位掩码相交的覆盖 key（例如 `{3: ...}` 同时命中 TOUCH | KBM）已实现，但冒烟只覆盖了精确 key。

## P3：ButtonSpecs / ActionButtonHost / ActionButton

**目标**：把「通道 → 按钮」的映射从触屏控件层挪到 Godot 侧数据表，一个通道可以出多个按钮；按钮作为 HudWidget 进入托盘槽位，复用安全区 / 缩放 / 方案覆盖。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/input/button_specs.gd`（新增） | `BY_CHANNEL` 映射表：jump / tool（两个）/ glide / flight（`flight_toggle`）/ interact / sprint 共 7 个按钮，预留 `BY_ACTION`、`fallback_spec()`、`specs_for_channels()`；`normalize_spec()` 归一化 id / label / action / mode / channel / tray / align / priority / schemes / scheme_overrides，缺省 `tray=RIGHT`、`align=END`、`priority=0`、`schemes=TOUCH` |
| `godot-project/ui/input/action_button_host.gd`（新增） | Node 宿主：从 `client.input_context` 读通道表（`channels()` / `has_channel()` / `is_channel_available()` / `channel_changed`），跳过 `axis2d`，按 ButtonSpecs 建按钮并注册进 HudRegistry；`refresh()` 补偿晚到的 client / context / 通道表，未知 key 时重拉表；`channel_changed` 更新显隐 / 置灰并立即释放不可用动作；菜单 / 切方案 / 失焦 / 退树 `release_all()`；`destroy()` 断信号并从注册表移除按钮；测试注入缝 `apply_channels()` / `set_channel_state()`；无 client 时 0 按钮不报错 |
| `godot-project/ui/widgets/action_button.gd` + `.tscn`（新增） | 自绘 HudWidget（`Control` 子类）：处理 `InputEventScreenTouch`（`index` 匹配，同时兼容 `InputEventMouseButton`），一个按钮只跟一根手指，按下 press / 抬起 release，`toggle` 0.1s 自动 release（用 generation 防止误释放下一次按下）；隐藏 / 失焦 / 退树 / 切方案 / detach 全部 release |
| `godot-project/ui/touch/touch_controls.gd` | 删掉动作按钮与通道表逻辑，只剩虚拟摇杆 + 视角区；`_release_all()` 只复位摇杆与视角手势 |
| `godot-project/ui/ui_root.gd` | 创建 `action_host` 并 `setup(self, registry)`；`enable_touch_controls` 关闭时不建；订阅客户端能力位变化做补偿刷新；`_exit_tree` 调 `destroy()` |
| `godot-project/test/ui_smoke_test.gd` | 新增 P3 检查：tool 双按钮、缺规格兜底、axis2d 不出按钮、模拟双指同按两按钮、松开互不影响、toggle 自动释放、通道不可用立即释放、菜单 / 切方案隐藏 |
| `godot-project/test/ui_gameplay_probe.gd` | `--gameplay` 探针新增端到端检查：`action_host` 存在、真实通道表下按钮数为 7、`tool` 两个；失败即 `PROBE FAIL` 非零退出 |

**关于「原生 Button 不行」的前提修正（verifier 实测，Godot 4.7.2）**：

原方案假设「`emulate_mouse_from_touch=false` 时原生 Button 收不到触摸、GUI 不支持多指」在 4.7.2 上**不成立**：ScreenTouch 直接触发 Button 的 `button_down` / `pressed` / `button_up`（BaseButton 原生处理 ScreenTouch，Viewport 按 touch index 维护 `gui.touch_focus`）；两根手指分别按两个 Button 各自独立，同一 Button 的第二根手指被忽略，drag 出矩形后 release 按 index 回投原控件。

因此**自绘不是对引擎能力的绕行，而是可选增强**，理由三条：释放时机可控（不可用 / 隐藏 / 菜单 / 失焦立即 release）、per-index 语义显式（多指同按各归各的）、与 HudWidget 契约一致（复用 tray / align / priority / schemes / scheme_overrides 与注册表）。结论：4.7.2 上原生 Button 也支持触摸与多指。

**收尾提交 `7c6f1ac`**：修正 `action_button.gd` 的注释口径（明确「原生 Button 4.7.2 也支持触摸与多指，自绘是三项增强、不是绕行引擎缺陷」），并给 `--gameplay` 探针补暂停段断言——`backend_paused` / `camera_blocked` 在暂停 / 恢复后不满足期望时 `PROBE FAIL` 非零退出（之前只打印不断言）。

**验证**：headless 冒烟用构造的 `InputEventScreenTouch` 覆盖按下 / 释放 / toggle / 双指同按；`--gameplay` 探针用真实通道表跑通「通道表 → ButtonSpecs → 7 个按钮（tool 2 个）」并有暂停断言；真实触摸事件细节见「仍未被验证的风险」。合并后 checks 见「测试结果」。

## P3b：交互提示键位文案

**目标**：交互提示不再硬编码「按 E」，键位文案跟随实际 InputMap 绑定与当前输入方案。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/widgets/interact_prompt.gd` | 按当前方案生成提示：触屏固定 `Tap`；键鼠取 InputMap 的 `interact` 动作第一个键鼠事件，键盘用 `OS.get_keycode_string(physical_keycode)` 取干净键名、鼠标按键用 `as_text()`，拼成 `[E] Interact`；取不到回落 `Interact`；方案变化时刷新 |
| `godot-project/test/ui_smoke_test.gd` | 新增 4 项行为检查：键鼠提示含 `Interact`、真实绑定 `[E]`、改键后 `[F]`、触屏 `Tap`；另有 2 项元素 / Label 存在性检查 |

**键位文案细节**：BevyInput 用 `physical_keycode` 注册动作，`InputEventKey.as_text()` 在 4.7 会返回 `E - Physical` 这类带后缀文本，所以键盘按键改用 `OS.get_keycode_string(physical_keycode)` 得到 `E`；鼠标按键仍用 `as_text()`。改键后提示跟着 InputMap 变。

**验证**：不改 Rust 动作表；一个动作绑定多个键鼠事件时只取第一个；提示固定绑 `interact` 动作，不按 ActionId 反查动作名（ActionId 仍只用于显示条件）。

## P4：group_id 同组互斥

**目标**：同一功能可以按方案准备不同形态的元素（例如桌面面板 / 手机抽屉），但同一时刻只出现一个。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/hud_widget.gd` | 新增 `@export var group_id: StringName`、`set_group_active(bool)`；可见性公式加入同组激活位 |
| `godot-project/ui/hud_registry.gd` | 新增 `_apply_groups(current)`：按组统计命中当前 scheme 的元素，只让 `effective_priority` 最高者激活；在 register / unregister / refresh_scheme 后重算 |
| `godot-project/test/ui_smoke_test.gd` | 新增 5 项检查：同组只激活一个、命中方案才参与竞争、priority 决定胜者 |

**范围说明**：本轮只实现注册表规则 + 冒烟测试，没有具体背包 / 面板界面落地。

**验证**：`tools/ui_check.sh` 通过，checks 由 72 增至 77。

## P5：混合设备防抖

**目标**：手机 / 平板接键盘、带触摸屏的笔记本、手柄 + 键鼠同时在线时，避免多个设备的事件互相抢 `scheme`。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/ui_root.gd` | 新增 `@export scheme_switch_cooldown=0.5`（仅输入事件驱动的切换受冷却）、`joypad_motion_threshold=0.35`（摇杆轴值低于阈值不算 GAMEPAD）、`mouse_motion_threshold=2.0`（逻辑像素，微动忽略）；`_set_scheme(next, from_input)` 区分程序化切换与输入切换；记录 `_last_scheme_switch_ms` |
| `godot-project/test/ui_smoke_test.gd` | 新增 7 项检查：冷却拦截输入切换但放过程序化切换、摇杆低于阈值不切、鼠标微动不切、超过阈值正常切 |

**验证**：`tools/ui_check.sh` 通过，checks 由 77 增至 84。

## P6：移动端入口（触屏暂停 / 触摸滑条 / 调试菜单）

**问题**：手机没有 Esc、没有 F3、没有物理键盘。实测（本机 Godot 4.7.2 headless 探针）：

- 暂停菜单只能靠 **Android 返回键**进，屏幕内没有入口；
- 暂停菜单按钮触摸可用，但 **UI SCALE 的 `HSlider` 拖不动**（按下 / 拖动 / 抬手后 value 恒为 0.8）——原生 `HSlider` 只处理鼠标事件，`emulate_mouse_from_touch=false` 时触摸不会合成鼠标；
- F3 调试层只在 `_unhandled_input` 判物理键，手机 / 手柄都没有入口。

**改动**：

| 文件 | 改动 |
| --- | --- |
| `godot-project/ui/widgets/pause_button.gd` + `.tscn`（新增） | 触屏暂停按钮（内置 HudWidget：`tray=TOP/END`、`schemes=TOUCH`、72×72）：自绘两条竖杠，按下 → `open_pause_menu()`；键鼠方案不显示 |
| `godot-project/ui/widgets/touch_slider.gd`（新增） | `TouchSlider extends HSlider`：监听 `gui_input` 信号，处理 `InputEventScreenTouch` / `ScreenDrag`，按触摸 x 换算 value；鼠标 / 键盘 / 手柄行为不变（不覆写内置控件的 C++ 输入处理） |
| `godot-project/ui/screens/pause_menu.gd` / `.tscn` | Settings 的 HSlider 换成 TouchSlider（行高 40→64）；新增 DebugButton / DebugPanel（`Debug Overlay: ON/OFF`、`Force TOUCH: ON/OFF`、Back），仅 `OS.is_debug_build()` 或 `force_show_debug` 时显示；提示按方案变 `Tap Resume` / `Esc to resume`；新增 `debug_overlay_requested` / `debug_scheme_requested` 信号 |
| `godot-project/ui/ui_root.gd` | 内置 HUD 注册 `pause_button.tscn`；连接暂停菜单的调试信号；抽出公开 `set_debug_overlay()`（F3 与 Debug 区共用）；打开菜单时同步 Debug 区状态 |
| `godot-project/test/ui_smoke_test.gd` | 新增 16 项检查：暂停按钮显隐 / 触摸打开菜单；滑条触摸变大 / 拖动变小 / 写回 `UiState.ui_scale`；Debug 区可见性 / 调试层开关 / 强制方案 / `Tap Resume` 提示 / release 隐藏 |

**验证**：`tools/ui_check.sh` 通过，checks 由 112 增至 128；`--gameplay` 探针 widgets 由 10 增至 11（多一个内置 PauseButton），按钮与暂停结论不变。

**边界**：只解决「入口可达性」，没有新增设置项（音量 / 灵敏度 / 摇杆死区 / 按钮布局要扩 `UiState` + 存档，属于另一轮）；Debug 区不做隐藏手势，只在 debug 构建出现。

## 测试结果

代码冻结后（HEAD = `db7dcae`，P6）实跑：

```text
$ GODOT_BIN=/tmp/Godot_v4.7.2-stable_linux.arm64 tools/ui_check.sh --gameplay
[ui_smoke] checks=128 failures=0
UI_SMOKE PASS

PROBE client=true widgets=11
PROBE action_host=true buttons=7 tool_buttons=2 channels=7
PROBE menu_open=true backend_paused=true camera_blocked=true
PROBE menu_closed=true backend_paused=false camera_blocked=false
PROBE PASS
```

- 冒烟：**128 项检查，0 失败**，`UI_SMOKE PASS`（2026-10-22，Godot 4.7.2 headless，aarch64）。
- 集成探针：**PROBE PASS**，真实通道表下 7 个动作按钮、`tool` 2 个，暂停前后 `backend_paused` / `camera_blocked` 正确。**本机 headless 跑通，不是真机验证。**
- 逐阶段 checks 增量：43 → 51（P0）→ 57（P1）→ 72（P2）→ 77（P4）→ 84（P5）→ 112（P3 + P3b）→ 128（P6）。

## 仍未被验证的风险

| 风险 | 说明 | 状态 |
| --- | --- | --- |
| P3 触摸结论（已实证部分） | verifier 实测 Godot 4.7.2：ScreenTouch 直接触发原生 Button 的 down / pressed / up（非鼠标模拟）；两指分别按两个 Button 各自独立；同一 Button 第二根手指被忽略；drag 出矩形后 release 按 index 回投原控件。自绘 `ActionButton` 的释放时机 / per-index 语义按此实现，headless 用构造 ScreenTouch 断言通过 | 已实证（headless / verifier） |
| 真机触摸事件细节 | headless 是 stub DisplayServer（窗口 64×64、视口正方形、无真实 DPR / 安全区、`is_touchscreen_available` 恒 false），不能替代真机：真机坐标缩放 / 断点 / 安全区、`InputEventScreenTouch.canceled=true`（系统取消触摸）时是否释放动作、真实多指乱序 / 丢帧、按钮 + 摇杆 + 视角三指同时按 | 未验证（需真机） |
| Android 真机安全区 | headless 下 `safe_insets` 恒 0，刘海 / 挖孔 / 手势条只能在真机验证 | 未验证 |
| 混合设备防抖阈值 | scheme 切换冷却 0.5s、摇杆阈值、鼠标微动忽略都是初值 | 未验证，需真机 / 桌面触屏机调参 |
| 移动端入口真机未验证 | 触屏暂停按钮与触摸滑条的命中 / 拖动手感、debug 包 Debug 区、Android 返回手势与系统取消触摸都未在真机验证；release 包隐藏 Debug 区也只在 headless（editor 构建恒 `is_debug_build()=true`）下间接验证 | 出 debug / release APK 各点一遍；未验证 |
| 交互意图抽象 | 只在 [[10-Godot-UI方案]] 的「交互意图抽象」一节写设计（原独立 12 号文档已并入），未实现；元素仍然直接处理设备事件 | 未实现（非本轮范围） |

## 不在本轮范围

- Rust 输入链路、量化、能力过滤、暂停：零改动。
- CJK 字体与本地化：UI 文案仍为英文。
- mod UI 加载器（manifest 驱动）：仍只有 `register_external` 入口。
- 设置项扩容：Settings 仍只有 UI SCALE；音量 / 灵敏度 / 摇杆死区 / 按钮布局要扩 `UiState` + 存档，属于后续轮次。
