---
title: Godot UI 方案
type: 架构文档
status: active
created: 2026-10-21
updated: 2026-10-22
tags:
  - 项目架构
  - UI
  - 输入
  - 移动端
source: "代码阅读 @ 本次 UI 适配"
---

# Godot UI 方案

一句话：**一套 UI 树 + platform / scheme / compact_level 三量分离 + 三条托盘行（top / mid / bottom）+ 左右两个竖列 + 中央 + 九个槽位 + HudWidget 契约（含 scheme_overrides），不做布局求解器。** 桌面、手机、平板、手柄共用同一棵树：platform 决定缩放目标 / 安全区 / 基础边距，scheme 决定显隐与重排，compact_level 决定隐藏哪些元素；触屏动作按钮走 ButtonSpecs 映射 + 自绘 ActionButton，触屏只是往 InputMap 里注入动作，不改 Rust 输入链路。

## 目标与边界

| 目标 | 做法 |
| --- | --- |
| 一套 UI 适配桌面 / 手机 / 平板 / 手柄 | 单 UI 树 + 输入方案位掩码显隐，不维护两套界面 |
| 分辨率与刘海屏自适应 | 托盘容器 + 断点 + 移动端目标逻辑宽度 + 安全区内缩 |
| 元素不写坐标 | HudWidget 只声明 tray / align / priority / schemes / compact_level，可按方案覆盖（scheme_overrides）与同组互斥（group_id） |
| mod UI 免费获得适配 | `register_external(scene, meta)` 注册进同一套槽位 |
| 触屏不新增输入链路 | 摇杆写 `Input.action_press`，视角走 `BevyInput.on_look_delta` |
| 暂停与玩法输入互斥 | `UiState.menu_depth` 门控提交，后端暂停走 `pause_bevy()` |

明确不做（本次边界）：

- 不做布局求解器、约束系统、绝对坐标布局；复杂排列由容器嵌套解决。
- 不做 desktop / mobile 两套场景或两套 HUD。
- 不在 HudWidget 里判断分辨率、写坐标、算安全区。
- 不在 Rust 侧识别触屏设备；触屏只是 InputMap 的动作来源之一。

## 目录与文件

| 路径 | 作用 | 状态 |
| --- | --- | --- |
| `godot-project/ui/ui_root.gd` / `ui_root.tscn` | UI 根（CanvasLayer），构建托盘树、安全区、缩放、断点、platform / scheme 检测与防抖、菜单 / 暂停、F3 调试层；创建 `ActionButtonHost` 并在客户端能力位变化时补刷新 | 已实现 |
| `godot-project/ui/ui_state.gd` | UI 全局静态状态，无 Autoload | 已实现 |
| `godot-project/ui/hud_widget.gd` | HudWidget 契约基类（Control）；`scheme_overrides` 按方案覆盖 tray / align / priority / min_size | 已实现 |
| `godot-project/ui/hud_registry.gd` | 托盘槽位注册表 + priority 排序 + `relocate()` 按方案重排 + 同组互斥 + mod 注册校验 | 已实现 |
| `godot-project/ui/input/button_specs.gd` | 通道 → 动作按钮规格映射表（`BY_CHANNEL` / 预留 `BY_ACTION` / `fallback_spec`） | 本轮新增 |
| `godot-project/ui/input/action_button_host.gd` | 动作按钮宿主：读通道表、创建 / 更新 / 显隐置灰 / 统一释放 | 本轮新增 |
| `godot-project/ui/widgets/action_button.gd` / `.tscn` | 自绘动作按钮（可选增强，原生 Button 也支持触摸），按触摸 index 处理按下 / 抬起 / toggle | 本轮新增 |
| `godot-project/ui/debug/hud_debug_overlay.gd` | F3 只读调试层（尺寸 / 缩放 / 安全区 / 断点 / 元素矩形 / 重叠告警） | 已实现 |
| `godot-project/ui/widgets/` | 内置 HUD：准星、血条、交互提示、触屏暂停按钮 | 已实现 |
| `godot-project/ui/widgets/touch_slider.gd` | 触摸可拖动的 `HSlider`（原生 HSlider 只认鼠标事件） | 本轮新增 |
| `godot-project/ui/widgets/pause_button.gd` / `.tscn` | 触屏暂停按钮（仅 TOUCH 方案显示，落 TOP / END 托盘） | 本轮新增 |
| `godot-project/ui/touch/touch_controls.gd` / `.tscn` | 触屏控件层：只剩左下虚拟摇杆 + 右侧视角区（动作按钮已移交 `ActionButtonHost`） | 已实现 |
| `godot-project/ui/touch/touch_joystick.gd` | 单指虚拟摇杆，写 `move_*` 动作强度 | 已实现 |
| `godot-project/ui/screens/pause_menu.gd` / `.tscn` | 暂停菜单：Resume / Settings（触摸滑条）/ Quit；debug 构建额外显示 Debug 区（调试层 / 强制方案） | 已实现 |
| `godot-project/bevy_client/camera/camera_rig.gd` | 相机：`camera_rig` 组、`set_ui_blocked()`、触屏 / 菜单不捕获鼠标 | 已实现 |
| `godot-project/test/ui_smoke_test.gd` | UI headless 冒烟（`extends SceneTree`），按 P0-P6 分阶段覆盖 | 已实现 |
| `godot-project/test/gen_class_cache.gd` | 无编辑器时生成 `class_name` 缓存 | 已实现 |
| `tools/ui_check.sh` | 一步跑完类名缓存 + UI 冒烟 | 已实现 |

`UiRoot` 的场景文件只挂脚本，整棵布局树在 `_build_layout()` 里用代码构建，避免场景文件与适配逻辑两头维护。

可选子场景按“存在才加载”处理（`ResourceLoader.exists`）：触屏控件、暂停菜单、内置 HUD 场景缺失时 UiRoot 照常启动，只是少对应部件。这让 UI 骨架与具体控件可以分开落地。`ActionButtonHost` 不是场景：`enable_touch_controls` 为真时由 UiRoot 在代码里创建，无 client 时按钮数为 0。

## 布局树

```text
UiRoot (CanvasLayer, layer=10, PROCESS_MODE_ALWAYS)
└ UiTree (Control, 全屏, MOUSE_FILTER_IGNORE, 内置 Theme)
  └ SafeArea (MarginContainer)        ← 安全区 + 边距只在这里加
    └ Trays (VBoxContainer)
      ├ TopTray (HBox)
      │  ├ TopBegin   (HBox, ALIGNMENT_BEGIN)
      │  ├ TopCenter  (HBox, ALIGNMENT_CENTER)
      │  └ TopEnd     (HBox, ALIGNMENT_END)
      ├ MidTray (HBox, 垂直 EXPAND_FILL)
      │  ├ MidBegin  (VBox)             ← 左竖列
      │  ├ MidCenter (CenterContainer)  ← 中央，吃掉剩余宽度
      │  └ MidEnd    (VBox)             ← 右竖列
      └ BottomTray (HBox)
         ├ BottomBegin / BottomCenter / BottomEnd
```

九个槽位全是容器：top / bottom 的六个是 `HBoxContainer`，`mid_begin` / `mid_end` 是 `VBoxContainer`，`mid_center` 是 `CenterContainer`。元素加进去由容器重排。所有托盘与槽位默认 `MOUSE_FILTER_IGNORE`，不拦输入，真正可交互的控件自己打开鼠标过滤。

动作按钮是普通 HudWidget（默认 `tray=RIGHT` / `align=END`），进 `mid_end` 右竖列、按注册顺序纵排（槽内整体垂直居中）；它们与内置 HUD 共用 `SafeArea` 和缩放，不再由触屏控件层手算坐标。

## 托盘与槽位契约

`HudWidget` 用两个枚举声明位置：

| 枚举 | 取值 |
| --- | --- |
| `Tray` | `TOP` / `BOTTOM` / `LEFT` / `RIGHT` / `CENTER` |
| `Align` | `BEGIN` / `CENTER` / `END` |

注册表据此落到 9 个槽位 key：

| Tray | Align | 槽位 key |
| --- | --- | --- |
| TOP | BEGIN / CENTER / END | `top_begin` / `top_center` / `top_end` |
| BOTTOM | BEGIN / CENTER / END | `bottom_begin` / `bottom_center` / `bottom_end` |
| LEFT | 忽略（只有一列） | `mid_begin` |
| RIGHT | 忽略（只有一列） | `mid_end` |
| CENTER | 忽略 | `mid_center` |

LEFT / RIGHT 各只有一列，Align 不改变落点，只影响同槽内的排序。

槽内排序按当前方案的 `effective_priority()`：`BEGIN` 槽里 priority 越大越靠屏幕边缘（`top_begin` 越靠左、`bottom_begin` 越靠左）；其余槽相反，priority 越大越靠近 `END` 边。冒烟测试用两个 TOP+END 元素验证了“大优先级贴右边缘”。

注册规则（`HudRegistry.register`）：

1. `widget_id` 为空时退回节点名；为空且无节点名直接拒绝。
2. 重复 id 拒绝，不静默覆盖。
3. `tray` 取 `CENTER` 或未知值时都落 `mid_center`；只有注册表还没 `setup()`（槽位不存在）时才因取不到槽位而拒绝。
4. 插入槽位后调 `attach(ui_root)`，再按当前方案 `relocate()`（槽位 / 排序 / min_size）并重算同组激活，最后广播 `scheme` / `compact_level` / 菜单状态。`refresh_scheme()` 同样先重排再通知元素。

元素不写任何坐标；要新位置就声明 tray + align，要次序就调 priority。

## 自适应：platform / scheme / compact_level 三量分离

三个量各管一段，互不替代，也**不互相反推**：

| 量 | 取值 | 静态 / 动态 | 决定什么 |
| --- | --- | --- | --- |
| `platform` | `DESKTOP` / `PHONE` / `TABLET` | 静态（启动与缩放重算时写一次） | 缩放目标、安全区计算、基础边距 |
| `scheme` | `TOUCH` = 1 / `KBM` = 2 / `GAMEPAD` = 4（位掩码） | 动态（输入事件） | 元素显隐（`schemes`）、`scheme_overrides` 重排、动作按钮映射 |
| `compact_level` | `0` / `1` / `2` | 动态（逻辑宽度） | 哪些元素因太窄而隐藏 |

铁律：**边距与缩放看 `platform`，不看 `scheme`。** 平板接键盘时 `platform` 仍是 `TABLET`、`scheme` 变成 `KBM`，界面不该因为输入方案抖动而跳边距或跳缩放。`UiRoot._update_platform()` 在 `_apply_scale()` 开头写 `UiState.platform`：

| 判定 | platform |
| --- | --- |
| 桌面（非 mobile） | `DESKTOP` |
| mobile 且屏幕对角线 >= 7 英寸 | `TABLET` |
| mobile 且更小 | `PHONE` |
| `debug_force_mobile` | `PHONE`（它模拟手机缩放公式） |

### 1. 缩放：content_scale_factor

`project.godot` 的基准视口是 1920×1080，窗口覆盖 1280×720，拉伸模式 `canvas_items` + `expand`；逻辑尺寸随窗口变化，不由拉伸裁切。移动端固定横屏（`window/handheld/orientation=0`），所以“手机目标逻辑宽 900”按横屏宽度算。

| 场景 | 计算 |
| --- | --- |
| `platform == DESKTOP` | `_base_factor = 1.0`，只有用户缩放生效 |
| `platform == PHONE` / `TABLET` | `stretch = min(win.x / 1920, win.y / 1080)`；`target` 手机 900 / 平板 1280；`base = clamp(win.x / (target * stretch), 1, 8)` |
| 用户缩放 | `set_ui_scale(v)`，`v` 夹在 0.8–1.3 |
| 最终值 | `factor = clamp(base * user, 0.5, 8)`，写入 `Window.content_scale_factor` 与 `UiState.ui_scale` |

手机 / 平板按屏幕对角线长度判定：`screen.length() / screen_get_dpi() >= 7 英寸` 走平板目标宽度 1280，否则 900。目标是把逻辑宽收敛到 900（手机）/ 1280（平板）附近：屏幕比 16:9 更宽时 `stretch` 由高度决定、正好命中目标；更高的比例会按 `stretch` 略放大。HUD 元素用“设计单位”写尺寸，就能在两种设备上观感接近。

桌面不套 `content_scale_factor`，改窗口大小只改变逻辑宽高（由断点接管）。

`@export var debug_force_mobile` 强制走移动端公式：headless 冒烟用它验证“900 逻辑宽 → 紧凑等级 1”，桌面调它也能预览移动端断点与安全区行为（安全区仍由平台给值，桌面为 0）。

### 2. 断点：compact_level

断点看逻辑宽度（设计单位），不是物理像素：

| 逻辑宽度 | compact_level | 含义 |
| --- | --- | --- |
| `>= 1000` | 0 | 正常 |
| `760 <= w < 1000` | 1 | 紧凑 |
| `w < 760` | 2 | 极窄 |

逻辑尺寸从 `get_viewport().get_visible_rect().size` 取，且**延迟一帧再读**：`content_scale_factor` 生效是延迟的，尺寸变化当帧读到的还是旧值。变化时写 `UiState.compact_level`、调 `registry.refresh_compact(level)` 并发 `compact_changed`。

元素用 `compact_level` 声明“紧凑到几级就隐藏”：`0` 表示永不因紧凑隐藏，`1` 表示 level >= 1 时隐藏，依此类推。隐藏而不是缩小，避免挤成不可读的小字。

### 3. 安全区：safe_insets

只在移动端内缩（Android edge-to-edge 下必须做，否则刘海、挖孔、手势条会盖住 HUD）：

1. `DisplayServer.get_display_safe_area()` 拿到物理像素安全矩形，`DisplayServer.window_get_size()` 拿窗口物理尺寸。
2. `px_per_unit = win_size.x / UiState.logical_width`，把像素换算回设计单位。
3. 四边内缩 `insets = (left, top, right, bottom)`，各夹到 `>= 0`，写 `UiState.safe_insets`。
4. 应用方式只有一处：`SafeArea` 这个 `MarginContainer` 的四边 margin = 基础边距 + 对应 inset。
5. 触屏控件挂在 `SafeArea` 之外（贴屏幕物理边缘），所以它的 layout 自己读 `UiState.safe_insets` 避让：摇杆避开左 / 下 insets（视角区仍占满右侧整高，它是拖拽区、不承载按钮）。动作按钮已改为 HudWidget 进托盘槽位，安全区由 `SafeArea` 统一处理，不再由触屏层手算。安全区更新后 UiRoot 调 `touch_controls.refresh_layout()` 重排一次；移动端 insets 要延迟一帧才准，所以重排不能省。

基础边距看 `platform`：非 `DESKTOP`（手机 / 平板）`TOUCH_PAD = 16`，`DESKTOP` `EDGE_PAD = 24`（设计单位）。桌面恒 `safe_insets = 0`。

三个量互相独立：缩放决定 1 个设计单位等于多少像素，安全区决定容器往内缩多少，断点决定哪些元素该藏。**唯一叠加点**是 `SafeArea` 的 margin（含 inset），HUD 元素自身永远从容器边缘开始排。

## HudWidget 契约

```gdscript
class_name HudWidget
extends Control

@export var widget_id: StringName = &""
@export var tray: Tray = Tray.TOP
@export var align: Align = Align.END
@export var priority: int = 0
@export_flags("Touch:1", "KBM:2", "Gamepad:4") var schemes: int = 7
@export var compact_level: int = 0
## 同组互斥：同组里命中当前 scheme 的元素只激活 priority 最高者。
@export var group_id: StringName = &""
## 按方案覆盖位置：key 是 Scheme 的值（1/2/4），value 可含
## tray / align / priority / min_size（Vector2）。
@export var scheme_overrides: Dictionary = {}
```

生命周期钩子：

| 时机 | 回调 | 用途 |
| --- | --- | --- |
| 注册进槽位 | `attach(ui_root)` → `_on_attach()` | 连接 client 信号、拿 `ui_root` |
| 输入方案变化 | `refresh_scheme(current)` → `_on_scheme_changed()` | 应用 `effective_min_size()`、换图标 / 文案 |
| 同组竞争结果 | `set_group_active(bool)` | 只改显隐，不影响业务数据 |
| 紧凑等级变化 | `apply_compact(level)` → `_on_compact_changed()` | 换布局或提示 |
| 业务数据有无 | `set_data_visible(v)` | 没有 HEALTH 载荷就隐藏血条 |
| 菜单开 / 关 | `refresh_menu()` | 统一隐藏 / 恢复 |

可见性由基类统一算，子类不各写一套：

```text
visible = 业务数据可见 and 当前方案命中 schemes and 未达 compact_level and 同组激活 and 菜单未打开
```

`schemes` 是位掩码：`1` 触屏、`2` 键鼠、`4` 手柄，默认 `7` 全方案可见。

### scheme_overrides：按输入方案覆盖位置

同一个元素在不同方案下可以换槽位、换对齐、换次序、换最小尺寸，而不需要维护多份场景或让元素自己判断设备。覆盖只影响**放置**，不改变 `schemes` 的显隐语义（方案不在 `schemes` 里仍然整体隐藏）。

| key | 类型 | 说明 |
| --- | --- | --- |
| `tray` | `HudWidget.Tray` | 覆盖托盘 |
| `align` | `HudWidget.Align` | 覆盖对齐 |
| `priority` | int | 覆盖槽内排序 |
| `min_size` | Vector2 | 覆盖 `custom_minimum_size`；切回无覆盖方案时恢复默认值 |

key 先按精确值匹配（`{1: ...}`），找不到再找**位掩码相交**的 key（例如 `{3: ...}` 同时命中 TOUCH | KBM），因此组合方案也能共享一份覆盖。位掩码相交这条路径已实现，但冒烟只覆盖精确 key，未单独断言。

元素不直接读字典，而是通过基类方法取**当前方案生效后**的值：

| 方法 | 返回 |
| --- | --- |
| `effective_tray(scheme)` | 覆盖后的 tray，无覆盖用 `tray` |
| `effective_align(scheme)` | 覆盖后的 align |
| `effective_priority(scheme)` | 覆盖后的 priority |
| `effective_min_size(scheme)` | 覆盖后的最小尺寸；默认值在首次需要时记下（不放在 `_ready()`，避免子类覆写 `_ready()` 时跳过） |

`HudRegistry.refresh_scheme()` 里先对每个元素调 `relocate(widget, current)`：按新方案算 `effective_tray()` / `effective_align()` / `effective_priority()`，从当前父容器移除后按新落点重新插入（**同槽也会移除再插一次**，否则新 priority 不会重排），返回父容器是否真的变了；随后元素的 `refresh_scheme()` 把 `effective_min_size()` 写回 `custom_minimum_size` 并触发 `_on_scheme_changed()`；最后 `_apply_groups()` 重算同组激活。顺序不能反：元素在 `_on_scheme_changed()` 里读到的布局必须已经是新方案的。

示例（血条在桌面贴左上、触屏挪到右下并缩小）：

```gdscript
scheme_overrides = {
    UiState.Scheme.TOUCH: {
        "tray": HudWidget.Tray.RIGHT,
        "align": HudWidget.Align.END,
        "priority": 10,
        "min_size": Vector2(180.0, 24.0),
    },
}
```

`register_external()` 的 meta 也接受 `scheme_overrides`；640×480 的尺寸上限对**每个方案**的 `min_size` 都生效，任一方案超限即拒绝注册。

### 同组互斥：group_id（P4）

多个元素声明同一个 `group_id` 时，它们竞争同一个"激活位"：每次注册 / 注销 / 方案变化后，注册表按组统计**命中当前 scheme** 的元素，只让 `effective_priority` 最高的那个 `set_group_active(true)`，其余 `set_group_active(false)` 直接隐藏（不是置灰）；`group_id` 为空不参与。用途是让同一功能在不同方案下准备不同形态的控件，又不至于同时出现两份。

## UiState 静态状态

`ui/ui_state.gd` 是纯静态状态（`class_name UiState extends RefCounted`），**不用 Autoload**：任何场景挂上 UiRoot 就能用，不依赖 `project.godot` 的 autoload 顺序，headless 冒烟也不必初始化引擎单例。

| 字段 | 类型 | 默认 | 含义 |
| --- | --- | --- | --- |
| `platform` | int | DESKTOP | 静态设备类别：DESKTOP / PHONE / TABLET；决定缩放目标、安全区、基础边距 |
| `scheme` | int | KBM | 输入方案位掩码：TOUCH=1 / KBM=2 / GAMEPAD=4 |
| `menu_depth` | int | 0 | 菜单栈深度，> 0 表示暂停中 |
| `backend_paused` | bool | false | 是否已经向后端发过 pause |
| `ui_scale` | float | 1.0 | 当前 `Window.content_scale_factor` |
| `safe_insets` | Vector4 | 0 | 安全区内缩，设计单位 (left, top, right, bottom) |
| `logical_width` / `logical_height` | float | 1920×1080 | 逻辑可用尺寸（设计单位） |
| `compact_level` | int | 0 | 0 正常 / 1 紧凑 / 2 极窄 |

写方只有 UiRoot（`platform` 在 `_apply_scale()` 开头写，`scheme` 在方案检测 / 切换时写）；读方是 HUD 元素、CameraRig、BevyClient。辅助方法：

| 方法 | 用途 |
| --- | --- |
| `has_scheme(v)` | 方案位命中 |
| `set_scheme(v)` | 写方案，返回“是否真的变了”供调用方决定刷新 |
| `is_menu_open()` / `set_menu_open(b)` | 菜单深度 |
| `accepts_gameplay_input()` | `menu_depth == 0`，BevyClient 每帧据此决定是否提交输入 |
| `mouse_look_allowed()` | 菜单未开且非触屏方案 |
| `reset()` | 全部归默认（测试 / 退出场景用） |

## 输入方案检测

`UiState.scheme` 是位掩码（`TOUCH = 1`、`KBM = 2`、`GAMEPAD = 4`），当前实现一次只记录一个来源，切换时整体覆盖。

`UiRoot` 在 `_input` 里看事件类型判定：

| 事件 | 判定 |
| --- | --- |
| `InputEventScreenTouch` / `InputEventScreenDrag` | TOUCH |
| `InputEventJoypadButton` / `InputEventJoypadMotion` | GAMEPAD；摇杆轴值低于 `joypad_motion_threshold` 不算（P5 防抖） |
| `InputEventKey` / `InputEventMouseButton` | KBM |
| `InputEventMouseMotion` | KBM，但 `device == DEVICE_ID_EMULATION` 的合成鼠标事件、以及位移小于 `mouse_motion_threshold` 的微动直接忽略（P5 防抖） |

初始方案：`OS.has_feature("mobile")` 或 `DisplayServer.is_touchscreen_available()` 为 TOUCH，否则 KBM。

忽略“触屏模拟鼠标”很关键：`project.godot` 已设 `pointing/emulate_mouse_from_touch=false`，但部分平台 / 系统仍可能发合成事件；一旦把合成鼠标当真鼠标，触屏上会瞬间跳回键鼠方案，触屏控件消失。

方案真的变化时（`UiState.set_scheme` 返回 true 才继续）：

1. 重算缩放与安全区（两者都只看 `platform`，不看 scheme）。
2. 触屏控件显隐。
3. 相机是否被 UI 挡住（触屏方案恒 true）。
4. `registry.refresh_scheme(scheme)`：先 `relocate()` 重排，再广播给所有元素。
5. 发 `scheme_changed(scheme)`。

### 混合设备防抖（P5）

手机 / 平板接鼠标键盘、带触摸屏的笔记本、手柄 + 键鼠同时在线时，多个设备的事件会来回抢 `scheme`：手还在触屏上，鼠标微动一下就把方案切回 KBM，触屏控件消失再闪回来。本轮加三道防抖：

| 防抖 | 行为 |
| --- | --- |
| 方案切换冷却 | `@export var scheme_switch_cooldown: float = 0.5`。**只对来自输入事件的切换生效**（`_set_scheme(next, true)`）：初始化 / 代码显式 `_set_scheme()` 不受冷却限制，否则冒烟与菜单联动会被卡住 |
| 摇杆阈值 | `@export var joypad_motion_threshold: float = 0.35`：`InputEventJoypadMotion` 的 `abs(axis_value)` 低于它不算「用了手柄」，避免漂移反复触发 |
| 鼠标微动忽略 | `@export var mouse_motion_threshold: float = 2.0`（逻辑像素）：`InputEventMouseMotion.relative.length()` 低于它直接忽略；`DEVICE_ID_EMULATION` 的合成鼠标事件本来就忽略 |

触摸（`ScreenTouch` / `ScreenDrag`）、按键、鼠标按键、手柄按键不受幅度阈值限制，只受冷却限制。

防抖不改变「`scheme` 单值覆盖」模型：多个来源同时输入时仍是最近的事件获胜，只是加了时间与幅度门槛。真机上的阈值属于未验证项（见「待办与风险」）。

## 触屏输入映射

触屏不新增输入链路，只把触摸翻译成现有的 InputMap 动作与视角位移。控件层在 `ui/touch/`，由 UiRoot 实例化到 UiTree 最底层（放在 HUD 之下，不挡血条与提示），父链反查 UiRoot 并监听 `scheme_changed` / `menu_changed`。**控件层只剩摇杆 + 视角区**：动作按钮已交给独立的 `ActionButtonHost` / `ActionButton`（见下一节）。

### 控件布局

| 部件 | 位置 / 尺寸 | 行为 |
| --- | --- | --- |
| 虚拟摇杆 `VirtualJoystick`（`touch_joystick.gd`） | 左下角，底圈半径 140 设计单位 | 只跟踪落在摇杆上的第一根手指（按 `event.index` 匹配）；死区 0.15 + 死区外线性重映射在控件内完成 |
| 视角区 `LookArea` | 右侧 55% 宽度、整屏高 | 记录从视角区按下的手指 index 集合，拖动按 index 归属 |
| 动作按钮 `ActionButtonHost` + `ActionButton` | 默认 `tray=RIGHT` / `align=END`，单按钮最小 96×96 | 按 ButtonSpecs 生成，缺规格的通道走兜底；一个通道可以有多个按钮（`tool` 两个） |

控件只在 TOUCH 方案且菜单关闭时可见；隐藏、切走方案、打开菜单、窗口失焦、退出树时触屏层 `_release_all()` 只复位摇杆与视角手势，动作按钮由 `ActionButtonHost` 在同样时机 `release_all()`，避免“手指抬起事件丢失”造成粘键。

### 映射链路

| 触摸控件 | 写入口 | 落点 |
| --- | --- | --- |
| 虚拟摇杆 | 按归一化方向拆成 4 个 `move_*` 动作的强度，`Input.action_press(action, strength)` / `action_release(action)` | `BevyInput.collect_and_submit()` 的 `Input.get_vector` 与 `is_action_pressed` 原样读到 |
| 视角区 | `ScreenDrag.relative × look_sensitivity` → `apply_look_delta()` → `BevyInput.on_look_delta(delta)` | 与鼠标位移同一条 `look_delta` 通道 → `quantize_look` / LookChannel |
| 动作按钮 | 按 ButtonSpecs 的 `spec.action` 写 `Input.action_press` / `action_release`；`toggle` 模式点按后 0.1s 自动 release，形成一次完整边沿 | 与键盘、手柄走同一套 `held / pressed / released` |

动作按钮的生成方式：**按 ButtonSpecs 生成，缺规格的通道走兜底**。`ActionButtonHost` 从 `BevyInputContext.channels()` 拉通道表，跳过 `mode == "axis2d"`（移动归摇杆），每个通道查 `ButtonSpecs.BY_CHANNEL`；查不到规格就用 `fallback_spec`（`key` + `actions[0]`）兜底，不再让控件层自己维护「通道 → 按钮」的隐式映射。`channel_changed` 里用 `present` 控制显隐、`available` 控制置灰，并在不可用时立即释放该动作。按钮文案来自规格的 `label`，缺省退回 `key.replace("_", " ").capitalize()`。

关键约定：

- 暂停闸：视角区先查 `UiState.accepts_gameplay_input()`，菜单期间不注入视角。
- 摇杆只对 `InputMap.has_action()` 的动作注入，动作名来自 BevyInput 从 game-core 注册的动作表。
- 多指：摇杆只跟第一根手指并按 index 匹配；视角区用 `Dictionary[index]` 记录；动作按钮由 `ActionButton` 自己按 `InputEventScreenTouch.index` 归属。verifier 已实测（4.7.2）：两指各按一个按钮互不影响、同一按钮第二根手指被忽略；真实多指乱序 / 丢帧与「按钮 + 摇杆 + 视角」三指同按仍需真机验证。
- `emulate_mouse_from_touch=false`，且采集层忽略 `DEVICE_ID_EMULATION` 的合成鼠标事件。
- `on_look_delta` 是 UI 注入视角的正式入口；`touch_controls.gd` 仍保留 `on_mouse_motion` 兜底（前向兼容），两条入口都只往同一个 `_look_accum` 里加。

这条设计的收益：Rust 侧零改动，暂停、能力过滤、量化、历史全部复用；代价是触屏输入必须落在已注册的 InputMap 动作上，没有“触屏专用动作”。

## 动作按钮：ButtonSpecs / ActionButtonHost / ActionButton

本轮把动作按钮拆成三层：**映射表 / 宿主 / 控件**。自绘 `ActionButton` 是**可选增强，不是对引擎能力的绕行**——Godot 4.7.2 上原生 `Button` 本身就支持触摸与多指（详见下面「触摸实现方式」）。拆三层的直接收益是：映射从控件层挪成数据表、一个通道可以出多个按钮（`tool` 通道的 `primary_tool` / `secondary_tool`）、释放时机与 per-index 语义由自己掌握。

| 层 | 文件 | 职责 |
| --- | --- | --- |
| 映射表 | `ui/input/button_specs.gd`（`ButtonSpecs`） | 通道 / 动作 → 按钮规格，纯数据 |
| 宿主 | `ui/input/action_button_host.gd`（`ActionButtonHost`） | 读通道表、创建 / 更新 / 销毁按钮、显隐置灰、统一释放 |
| 控件 | `ui/widgets/action_button.gd` / `.tscn`（`ActionButton`） | 自绘按钮，按触摸 index 产生 press / release |

### ButtonSpecs

映射表放在 **Godot** 侧（game-core 的通道表只给 key / mask / mode / actions，不含按钮布局与文案）：

- `BY_CHANNEL`：通道 key → 规格数组。一个通道可以给多个按钮（`tool` 给 `primary_tool` / `secondary_tool` 两个）。
- `BY_ACTION`：预留按动作名查规格，当前为空。
- `fallback_spec`：通道查不到规格时兜底，用 `key` 与 `actions[0]` 生成一个按钮。

当前 6 个显式通道（共 7 个按钮）：

| 通道 | 按钮 | action |
| --- | --- | --- |
| `jump` | Jump | `jump` |
| `tool` | Use / Alt | `primary_tool` / `secondary_tool` |
| `glide` | Glide | `glide` |
| `flight` | Fly | `flight_toggle` |
| `interact` | Interact | `interact` |
| `sprint` | Sprint | `sprint` |
| 缺规格通道（mod / 新通道） | 兜底按钮 | `actions[0]` |

规格字段归一化：

| 字段 | 说明 | 缺省 |
| --- | --- | --- |
| `id` | 按钮唯一 id（HudWidget.widget_id） | 由通道 + action 拼 |
| `label` | 英文文案 | `key.replace("_", " ").capitalize()` |
| `action` | 要注入的 InputMap 动作 | 通道的 `actions[0]` |
| `mode` | `button` / `hold` / `toggle` | 通道的 mode |
| `channel` | 来源通道 key | 通道 key |
| `tray` / `align` / `priority` | 放置（作为 HudWidget 注册） | `RIGHT` / `END` / 0 |
| `schemes` | 显隐位掩码 | `TOUCH` |
| `scheme_overrides` | 按方案覆盖位置 | 空 |

改按钮布局 / 文案只改这张表，不动控件与 Rust；代价是 game-core 通道表变化时 Godot 侧要同步更新。

### ActionButtonHost

`ActionButtonHost` 是普通 `Node`，不继承 HudWidget；它把每个按钮作为 HudWidget 注册进注册表，从而免费获得托盘、方案显隐、`scheme_overrides`、安全区与缩放适配。

- `setup(ui_root, registry)` 绑定 UI 根与注册表；
- 从 `client.input_context.channels()` 拉通道表，跳过 `axis2d`（移动归摇杆）；
- 订阅 `channel_changed(key, present, available)`：`present` 控制显隐，`available` 控制置灰，不可用时立即释放该动作；
- **晚到补偿**：`refresh()` 重新绑 client 并重拉通道表，覆盖「client 晚于 `UiRoot._ready`」「`input_context` 晚于 client 绑定」「`channels()` 晚于 context 就绪」三种时序；UiRoot 在 client 的 `input_capabilities_changed` 时调用它；
- `channel_changed` 收到当前表里没有的 key 时不丢弃，而是重拉一次通道表并补上这次边沿（否则晚绑定时按钮永远补不回来）；
- 测试注入缝：`apply_channels(rows)` / `set_channel_state(key, present, available)`，headless 冒烟不依赖真实后端；
- 菜单打开、切走 TOUCH 方案、窗口失焦、退出树时统一 `release_all()`，避免手指抬起事件丢失造成粘键；
- `destroy()` 释放动作、断信号、把按钮从注册表移除（宿主节点本身随 UiRoot 释放，`destroy()` 不 free 自己）。

### ActionButton

`ActionButton` 是自绘 `HudWidget`（`Control` 子类，不是原生 `Button`；场景文件 `action_button.tscn` 只挂脚本）：

- 自己处理 `InputEventScreenTouch`，按 `index` 归属手指，按下写 `Input.action_press(spec.action)`，抬起写 `action_release`；
- `toggle` 模式点按后 0.1s 自动 release，形成一次完整边沿；
- 不可用 / 隐藏 / 退树时确保释放对应动作；
- 视觉自己画，不依赖主题的 Button 样式。

**触摸实现方式**：`project.godot` 设了 `pointing/emulate_mouse_from_touch=false`，触摸事件直接进 GUI；按钮 `mouse_filter=STOP`，只吃落在自己矩形内的触摸。`ActionButton` 在 `_gui_input()` 里处理 `InputEventScreenTouch`（同时处理 `InputEventMouseButton` 兼容桌面调试 / headless），一个按钮只跟踪一根手指、按 `index` 匹配。

**关于原生 Button（verifier 实测，Godot 4.7.2）**：

- `InputEventScreenTouch` 会直接触发原生 `Button` 的 `button_down` / `pressed` / `button_up`，不是鼠标模拟——`BaseButton` 原生处理 ScreenTouch，`Viewport` 按 touch index 维护 `gui.touch_focus`；
- 两根手指分别按两个 Button，各自 down / up 独立；同一 Button 上的第二根手指被忽略（只认第一根）；手指拖出控件矩形后 release 仍按 index 回投到原控件；
- 运行时改 `ProjectSettings` 的 `emulate_mouse_from_touch` 无效（引擎启动时读一次）；只有 `Input.emulate_mouse_from_touch` 在 `Input.parse_input_event` 路径才产生合成鼠标。本项目 `emulate=false` 无需改，也不影响原生 Button 与自绘 Control。

**既然原生 Button 支持触摸，为什么还自绘**（三条理由，不是「Button 做不到」）：

1. **释放时机可控**：通道不可用 / 元素隐藏 / 菜单打开 / 窗口失焦时立即 release，不等 GUI 的按钮状态机；
2. **per-index 语义显式**：一个按钮只跟一根手指，多指同按时各按钮独立、互不串扰；
3. **与 HudWidget 契约一致**：直接复用 tray / align / priority / schemes / scheme_overrides 与注册表，不引入第二套宿主。

结论：自绘是可选增强，不是对引擎能力的绕行；4.7.2 上原生 Button 也支持触摸与多指。

**仍需真机验证**（headless 是 stub DisplayServer：窗口 64×64、视口正方形、无真实 DPR / 安全区、`is_touchscreen_available` 恒 false，不能替代真机）：真机坐标缩放 / 断点 / 安全区；`InputEventScreenTouch.canceled=true`（系统取消触摸）时是否释放动作；真实多指乱序 / 丢帧；按钮 + 摇杆 + 视角三指同时按。

## 交互意图抽象（设计，未实现）

一句话：把「鼠标悬停 / 长按 / 手柄焦点」这类设备差异收进方案层，元素只声明并响应意图 **hint / select / confirm / context**，不判断设备。本节是设计说明，**本轮不实现**；已落地的 `scheme` / `scheme_overrides` / `ButtonSpecs` 见本文对应章节。

### 问题

元素现在能声明「在哪些方案下可见」（`schemes` 位掩码）和「在不同方案下怎么摆」（`scheme_overrides`），但**交互仍绑定具体事件**：

- 桌面：鼠标悬停高亮、左键确认、右键上下文；
- 触屏：没有悬停，用按下 / 长按 / 抬手；
- 手柄：没有指针，用焦点移动 + 确认键。

如果每个元素自己判 `UiState.scheme`、自己监听 `InputEventMouse` / `InputEventScreenTouch`，就会出现 N 份设备分支，和「元素不判断设备」的边界冲突，mod 也没法复用交互元素。

### 意图模型

四个意图，语义与触发方式解耦：

| 意图 | 含义 | 桌面（KBM） | 触屏（TOUCH） | 手柄（GAMEPAD） |
| --- | --- | --- | --- | --- |
| `hint` | 用户「看向 / 指向」某元素，元素可展示可操作性 | 鼠标悬停 | 手指按下（落在元素上） | 焦点移入 |
| `select` | 明确选中目标（可以无副作用） | 左键按下 | 手指按下 | 焦点移入后确认前 |
| `confirm` | 执行主动作 | 左键释放 / Enter | 抬手（短按） | 确认键（A） |
| `context` | 执行次要 / 上下文动作 | 右键 | 长按 | 上下文键（X） |

触发方式是**方案层配置**，不是元素逻辑：长按多少毫秒、按下是否先发 `select`、抬手是否要求仍在元素内，都由方案表决定；元素只看到意图。

### 元素契约

元素只实现意图回调（伪代码，未落地）：`on_intent(intent, payload) -> bool`（返回 true 表示消费），或拆成 `_on_hint() / _on_select() / _on_confirm() / _on_context()`。约束：

- 回调里只改自身视觉、只发游戏语义请求（例如 `Input.action_press`）；
- 不读 `UiState.scheme`、不判断 `InputEvent` 子类、不写坐标；
- 不消费的意图返回 false，由路由层继续路由或丢弃。

### 与 scheme / scheme_overrides 的关系

| 层 | 管什么 | 索引 | 现状 |
| --- | --- | --- | --- |
| `scheme`（位掩码 1/2/4） | 当前输入方案、可见性（`schemes`） | 位与 | 已实现 |
| `scheme_overrides` | 同一元素在不同方案下的**放置**：tray / align / priority / min_size | scheme 值 | 已实现 |
| 意图路由（本节） | 同一元素在不同方案下**如何被触发** | scheme 值 | 未实现 |

边界：

- `scheme_overrides` 是布局维度，意图抽象是交互维度；`scheme_overrides` 不会让元素知道该监听什么事件。
- `platform`（DESKTOP / PHONE / TABLET）不参与意图映射：它只决定缩放、安全区与基础边距。平板接键盘时 platform 仍是 TABLET、scheme 变成 KBM，意图按 scheme 表走。
- 意图不新增输入链路：最终仍落到 InputMap 动作 / `look_delta` / Rust 命令，Rust 侧零改动。

### 迁移路径

1. **现状（P3 已落地）**：`ActionButton` 是自绘 `HudWidget` 子类，自己按 `InputEventScreenTouch.index` 处理触摸；`ActionButtonHost` 负责通道 → 按钮的生命周期、显隐与释放。这不是因为「原生 Button 不支持触摸」——4.7.2 上原生 Button 支持触摸与多指，自绘是为了释放时机、per-index 语义与 HudWidget 契约一致。
2. **抽路由**：把触摸 / 鼠标 / 焦点统一翻译成意图，元素实现意图回调。`ActionButton` 是第一个消费者（触摸按下 = `select`，抬手 = `confirm`，长按 = `context`）。
3. **P3b 是 hint 的轻量版**：交互提示元素只声明「要一个提示」，文案由方案给——键鼠取 InputMap 第一个键鼠事件（键盘 `OS.get_keycode_string(physical_keycode)`，鼠标 `as_text()`），触屏显示 `Tap`。提示元素不判断设备。
4. **手柄**：接焦点导航后，`focus_entered` / `focus_exited` 映射 `hint` / `select`，`ui_accept` 映射 `confirm`。
5. **mod API**：意图路由稳定后再公开，避免 mod 绑死设备事件；当前 mod UI 的鼠标过滤仍走 Godot 原生 `mouse_filter`。

### 未解决问题

- 焦点导航与意图模型如何合流（Godot focus 自带 select 语义，是否复用）。
- 长按阈值、按下后滑动容差（算取消还是算 context）没有定值。
- 意图消费与冒泡：上层已消费的意图是否阻止下层；菜单打开时意图如何短路。
- 多点触控同时命中多个元素时的意图排序：P3 已实测「两指各按一个按钮互不影响、同一按钮第二根手指被忽略」，但意图层的多指优先级、真实多指乱序 / 丢帧仍未定义。
- 触屏按下即 `select` 时，是否先给 `hint`（避免视觉闪动）。
- 提示文案的来源与本地化（当前无 CJK 字体，文案只能英文）。

## 暂停与输入互斥

入口：

| 入口 | 实现 |
| --- | --- |
| `ui_cancel`（默认含 Esc，手柄返回键视 InputMap 配置） | `UiRoot._unhandled_input` 里 toggle，并 `set_input_as_handled()` |
| Android 返回键 | `NOTIFICATION_WM_GO_BACK_REQUEST` 同样 toggle |
| **触屏暂停按钮**（`PauseButton`，仅 TOUCH 方案） | 点击 → `UiRoot.open_pause_menu()`；手机没有 Esc，这是屏幕内入口 |
| 公开 API | `open_pause_menu()` / `close_pause_menu()` / `toggle_pause_menu()` |

打开菜单（`open_pause_menu`）：

1. `UiState.set_menu_open(true)`，`menu_depth = 1`。
2. 隐藏触屏控件（避免菜单上还能拖摇杆）。
3. `manager.pause_bevy()`（Rust 扩展方法），经 `BevyControlMsg::Pause` 让 runner 停 tick 并挂起输入暂存区；只有 manager 存在时才写 `UiState.backend_paused = true`（P0：纯 UI 预览 / headless 没有后端，不能显示一个没发出去的暂停）。
4. `camera_rig` 组里的相机 `set_ui_blocked(true)`：释放鼠标捕获，不再吃视角输入。
5. `registry.refresh_menu()`，所有 HudWidget 按 `UiState.is_menu_open()` 隐藏。
6. 显示暂停菜单，发 `menu_changed(true)`。

关闭时按相反顺序恢复：`menu_depth = 0`、隐藏菜单、`resume_bevy()`、相机恢复（触屏方案下仍为 blocked）、HUD 刷新、触屏控件按方案恢复。

暂停面板（`ui/screens/pause_menu.gd` / `.tscn`）是全屏半透明遮罩 + 居中面板（480 宽，`PROCESS_MODE_ALWAYS`）：

| 元素 | 行为 |
| --- | --- |
| Resume | 发 `resume_requested`，UiRoot 转成 `close_pause_menu()` |
| Settings | 展开 SettingsPanel，焦点移到 UI 缩放滑条 |
| UI SCALE | `TouchSlider`（`HSlider` + 触摸处理）0.8–1.3、步长 0.05，`ui_scale_changed` → `UiRoot.set_ui_scale()`，实时改 `content_scale_factor` |
| Back | 收起 Settings，焦点回到 Settings 按钮 |
| Quit | 直接 `get_tree().quit()`，**无二次确认** |
| Hint | 键鼠 / 手柄 “Esc to resume”，触屏 “Tap Resume”（打开菜单时按方案刷新） |
| Debug（仅 debug 构建） | 展开 Debug 区：`Debug Overlay: ON/OFF` 开关调试层、`Force TOUCH: ON/OFF` 强制输入方案、Back 返回 |

`open()` 时焦点落在 Resume，`close()` 时释放焦点，键盘 / 手柄可以直接操作菜单。Esc 只在 UiRoot 处理，菜单自己不再消费 `ui_cancel`。

玩法输入互斥有两道闸：

| 闸 | 位置 | 行为 |
| --- | --- | --- |
| 提交闸 | `BevyClient._process` | 只有 `UiState.accepts_gameplay_input()`（`menu_depth == 0`）才 `collect_and_submit()`；菜单期间**完全不提交**，resume 后从零开始 |
| 相机闸 | `CameraRig.set_ui_blocked()` | 菜单 / 触屏方案下不捕获鼠标、不消费视角 |

菜单期间不提交而不是“提交空帧”，是为了避免暂停时长把边沿事件（`just_pressed`）攒到恢复瞬间误触发。

`CameraRig` 的对应改动：进 `camera_rig` 组；`set_ui_blocked(true)` 时把 `client.look_requires_mouse_capture = false` 并释放自己捕获的鼠标，解除时按当前模式重新应用（触屏方案仍然不捕获）；`_unhandled_input` 不再消费 `ui_cancel`，触屏方案直接返回，键鼠只有点击窗口才重新捕获。这样可避免“Esc 打开菜单后鼠标仍被相机吃掉”和“触屏上光标还要求先点一下窗口”这两类问题。

`UiState` 里另有 `mouse_look_allowed()`（菜单未开且非触屏方案），供后续需要直接判断键鼠视角的代码使用。

## 移动端入口（P6）

手机 / 平板没有 Esc、没有 F3、也没有物理键盘，所以「进菜单」「调设置」「开调试层」三件事在触屏方案上都需要屏幕内入口：

| 需求 | 入口 | 设计要点 |
| --- | --- | --- |
| 暂停 | `PauseButton`（TOP/END，仅 TOUCH） | 与 HUD 同属 HudWidget，复用槽位 / 安全区 / 缩放；按下调 `open_pause_menu()`。Android 返回键同时保留 |
| 设置 | 暂停菜单 Settings 区 | `TouchSlider` 让 `HSlider` 支持 `ScreenTouch` / `ScreenDrag`；原生 HSlider 只认鼠标事件，`emulate_mouse_from_touch=false` 时触摸不会合成鼠标，所以不修就拖不动（实测确认） |
| 调试层 | 暂停菜单 Debug 区（仅 debug 构建） | `OS.is_debug_build()` 为真才显示：调试层开关、强制 TOUCH / KBM。F3 仍是键鼠快捷入口，两者共用 `UiRoot.set_debug_overlay()` |

实现约束：

- `TouchSlider` 监听 `gui_input` 信号而不是覆写 `_gui_input`：内置控件的 C++ 输入处理不受 GDScript 覆写影响，信号方式能拿到触摸事件又不影响鼠标 / 键盘 / 手柄。
- 调试入口**只在 debug 构建显示**（`force_show_debug` 仅供测试 / 手工预览），release 包不暴露；调试层本身仍是只读浮层，不参与布局、不拦输入。
- 暂停按钮与 ActionButton 一样自绘并自己按 `event.index` 跟一根手指，按下即触发（不要求抬手）。
- 真机仍有未验证项：真实触摸事件分发、Android 返回手势 / 系统取消触摸、触屏滑条手感（见「待办与风险」）。

## 内置 HUD 元素

四个内置元素都在 `ui/widgets/`，由 `UiRoot.BUILTIN_WIDGET_PATHS` 逐个加载注册：

| 元素 | 槽位 | 尺寸 | 显示条件 / 数据源 |
| --- | --- | --- | --- |
| `Crosshair` | `tray=CENTER, align=CENTER` | 24×24 | `input_look_changed` → `input_context.has_look()`；`_draw()` 画四条短臂，默认隐藏 |
| `HealthBar` | `tray=TOP, align=BEGIN, priority=20` | 260×30 | `listen_component(HEALTH)` + `controlled_entity_changed`；读 `current` / `max` 填 ProgressBar 与数值 Label |
| `InteractPrompt` | `tray=BOTTOM, align=CENTER` | 180×40 | `listen_component(INTERACTION)`；`enabled` 且 `action != 0` 才显示；键位提示按方案生成（键鼠 `[E] Interact`（`OS.get_keycode_string(physical_keycode)`）/ 触屏 `Tap`） |
| `PauseButton` | `tray=TOP, align=END, priority=50`，`schemes=TOUCH` | 72×72 | 无数据源；自绘暂停图标，触摸 / 鼠标按下 → `open_pause_menu()`；键鼠方案不显示 |

共同约定：

- 全部继承 `HudWidget`，不写坐标、不判断分辨率；只读元素 `mouse_filter = IGNORE`，`PauseButton` 是唯一可交互的内置元素（`STOP`，且只在 TOUCH 方案出现）。
- 全部信号驱动，不轮询；没有 client（纯 UI 预览 / headless 冒烟）时保持隐藏。
- 文案一律英文：`Interact`、`100 / 100`。原因是没有 CJK 字体资源，中文会显示方块（见风险）。

`InteractPrompt` 的键位提示已解决（P3b），**不改 Rust 动作表**：键鼠方案取 InputMap 的 `interact` 动作绑定的第一个键鼠事件——键盘事件用 `OS.get_keycode_string(physical_keycode)` 取干净键名（BevyInput 用 `physical_keycode` 注册动作，`InputEventKey.as_text()` 在 4.7 会返回 `E - Physical` 这类带后缀文本），鼠标按键用 `as_text()`——拼成 `[E] Interact`；触屏方案固定显示 `Tap`；取不到动作 / 事件时回落 `Interact`。

边界：提示固定绑定 `interact` 动作，**不按载荷里的 ActionId 反查动作名**（`INTERACTION` 载荷的 `action` 仍只用于 `action != 0` 的显示条件）；一个动作绑定多个键鼠事件时只取第一个，不做「E / Left Mouse」全量展示；触屏统一 `Tap`，不区分长按 / 点按；改键后提示跟随 InputMap（这是想要的行为）。

## mod UI 注册路径

```text
mod 提供 PackedScene（根节点继承 HudWidget）
  → UiRoot.register_external(scene, meta)
  → HudRegistry.register_external()
      ├ scene.instantiate()
      ├ 根节点不是 HudWidget → 告警 + queue_free，返回 null
      ├ meta 覆盖 id / tray / align / priority / schemes / compact_level / mouse_filter / scheme_overrides
      ├ register(widget)：重复 id 拒绝（未知 tray 落 mid_center）
      ├ 注册后量 get_combined_minimum_size() 与每个方案的 min_size
      └ 任一方案超过 640×480 → unregister + 返回 null
  → 进入对应槽位，自动获得方案 / 紧凑 / 菜单 / 安全区 / 缩放适配
```

meta 字段：

| key | 类型 | 说明 |
| --- | --- | --- |
| `id` | String | 槽内唯一 id，缺省用节点名 |
| `tray` | int | `HudWidget.Tray`；未知值落 `mid_center` |
| `align` | int | `HudWidget.Align` |
| `priority` | int | 槽内排序 |
| `schemes` | int | 位掩码 |
| `compact_level` | int | 紧凑隐藏等级 |
| `mouse_filter` | int | `Control.MouseFilter` |
| `scheme_overrides` | Dictionary | 按方案覆盖 tray / align / priority / min_size，规则同 HudWidget |

尺寸上限 640×480 是“防挤爆”保护，不是布局能力上限：超限直接拒绝并告警，避免 mod 把自己注册成整屏遮罩。检查对**每个方案**声明的 `min_size` 都生效（`all_min_sizes()`），只在 TOUCH 下超限的 mod 元素也会被拒。

**未做**：真正的 mod UI 加载器。目前只有注册入口与内置 `BUILTIN_WIDGET_PATHS`；`ModManifest` → 场景路径 → `register_external` 的发现 / 排序 / 卸载链路还没接（见 [[08-静态数据与Mod体系]] 的 Mod 阶梯）。

## 调试层（F3）

`ui/debug/hud_debug_overlay.gd` 是只读浮层，由 UiRoot 创建并 `setup(self)`，F3 切换显隐，可见时每 0.25s 刷新一次：

| 行 | 内容 |
| --- | --- |
| 状态 | `scheme` / `compact_level` / `ui_scale` |
| 尺寸 | 逻辑宽高与窗口实际尺寸 |
| 安全区 | `safe_insets` 四边 |
| 菜单 | `menu_open` / `backend_paused` |
| 输入能力 | `present`、`available`、`look` |
| HUD | 已注册元素数 + 每个可见元素的全局矩形；两两相交时打 `WARN overlap` |

它 `PRESET_FULL_RECT` + `MOUSE_FILTER_IGNORE`，不参与布局也不拦输入。人工调断点 / 安全区 / 槽位时靠它核对“元素没越界、没互相压住”。

## 测试矩阵

自动化（headless，无编辑器）：

| 项 | 覆盖 |
| --- | --- |
| `tools/ui_check.sh` | 生成类名缓存 + `.godot/extension_list.cfg`，再跑 `test/ui_smoke_test.gd`，失败非零退出 |
| UiRoot 加载 | `ui_root.tscn` 可实例化，9 个槽位齐全，内置 HUD 自动注册 |
| 注册与排序 | 内置注册、priority 排序、重复 id 拒绝 |
| 方案 / 紧凑 / 菜单 | 三种显隐条件与菜单开关后恢复 |
| mod 路径 | `register_external` 正例、重复 id 与超尺寸反例 |
| 尺寸与缩放 | 桌面 / `debug_force_mobile` 模拟移动端两种情况下逻辑宽度（1920 / 900）与断点等级 |
| 触屏控件 | 实例化、触屏方案可见、摇杆上推注入 `move_forward` 且 `Input.get_vector` 读到前进、`reset()` 释放动作、模拟 `safe_insets=(12,0,20,48)` 时摇杆避开左 / 下安全区 |
| P0 释放路径 | 无 manager 不写 `backend_paused`；`unregister` 后载荷回调与信号成对断开；超尺寸 mod 元素不留注册表 |
| P1 platform | 桌面 / 切触屏方案后 platform 不变；`debug_force_mobile` → PHONE 且边距为 `TOUCH_PAD`；关闭后回 DESKTOP |
| P2 方案覆盖 | 覆盖 tray / align / priority 后重排；`min_size` 覆盖与切回恢复；mod meta 的 `scheme_overrides`；每方案尺寸上限 |
| P3 动作按钮 | `tool` 生成两个按钮、缺规格通道走兜底、`axis2d` 不出按钮；模拟两指同时按住两个按钮各自生效、松开互不影响；`toggle` 自动释放；通道不可用时立即释放；菜单 / 切方案后隐藏 |
| P3b 交互提示 | 键鼠提示含 `Interact`、真实绑定 `[E] Interact`（键名来自 `physical_keycode`）、改键后变 `[F] Interact`；触屏提示为 `Tap` |
| P4 同组互斥 | 同组只激活 priority 最高者，不命中当前方案的元素不参与竞争 |
| P5 混合设备防抖 | 输入切换受冷却限制、程序化切换不受；摇杆低于阈值 / 鼠标微动不切方案；超过阈值正常切 |
| P6 移动端入口 | 触屏暂停按钮：KBM 隐藏 / TOUCH 可见、按下打开暂停菜单；触摸滑条：按右端数值变大、拖动变小、经 `ui_scale_changed` 写回 `UiState.ui_scale`；Debug 区：debug 构建可见、开关调试层、强制 TOUCH / KBM、提示改 `Tap Resume`、release 构建入口隐藏 |
| 暂停菜单 | Resume 信号经 UiRoot 关闭菜单、打开时隐藏触屏控件、UI 缩放滑条经 `set_ui_scale()` 生效并恢复 |
| 真实场景集成（可选 `--gameplay`） | 加载 `gameplay_test.tscn`：UiRoot 找到 BevyClient、注册内置 HUD、实例化触屏 / 暂停 / `ActionButtonHost`、CameraRig 入组；真实通道表 → ButtonSpecs → 按钮数为 7、`tool` 两个；`open_pause_menu()` 后 `backend_paused=true` 且 `camera._ui_blocked=true`，关闭后恢复 |

当前结果（2026-10-22，Godot 4.7.2 headless，aarch64）：

```text
[ui_smoke] checks=128 failures=0
UI_SMOKE PASS

PROBE client=true widgets=11
PROBE action_host=true buttons=7 tool_buttons=2 channels=7
PROBE menu_open=true backend_paused=true camera_blocked=true
PROBE menu_closed=true backend_paused=false camera_blocked=false
PROBE PASS
```

`--gameplay` 探针是**本机 headless** 跑通（已有 aarch64 `.so`），不是真机验证。新增检查项后以最后一次 `tools/ui_check.sh` 输出为准。

动作按钮的触摸、双指同按与 toggle 边沿现在有 headless 断言（构造 `InputEventScreenTouch` 直接调 `_gui_input`）；触屏暂停按钮与触摸滑条同样是构造 `InputEventScreenTouch` 断言；视角区多指归属、真实触摸分发与滑条手感仍靠真机 / 人工验证。

逐阶段的文件清单、提交与仍未验证项见 [[UI适配改造变更说明]]。

人工矩阵（每条都要过一遍）：

| 环境 | 期望 |
| --- | --- |
| 1280×720 键鼠 | 正常断点，HUD 贴边不越界 |
| 1920×1080 键鼠 | 正常断点，槽位留白一致 |
| 手柄 | 方案切到 GAMEPAD，触屏控件消失 |
| 手机横屏（逻辑宽 900，orientation=landscape） | compact_level=1，触屏控件可见，安全区内缩生效 |
| 平板（逻辑宽 1280） | 断点 0，元素不会被拉得过宽 |
| 700 宽窗口 | compact_level=2，声明的极窄元素隐藏 |
| Android 刘海 / 手势条 | HUD 不压安全区（**未在真机验证，见风险**） |
| Android 真机多指 | 动作按钮 + 摇杆 + 视角三指同时按各自独立；系统取消触摸（`canceled=true`）时动作释放、不粘键（**未在真机验证**） |
| 手机触屏入口 | 触屏暂停按钮可点、暂停菜单按钮可点、Settings 滑条可拖、debug 包里 Debug 区可用（**未在真机验证**） |

## 不做什么

- 不做布局求解器 / flex / 约束求解；容器 + 槽位就是全部布局能力。
- 不做 desktop 与 mobile 两套 UI 树。
- 不在 Rust 侧区分触屏输入源；触屏只是 InputMap 的另一个动作来源。
- 不在 HudWidget 子类里写坐标、分辨率判断、安全区计算。
- 不做 UI 动画、转场、UI 音效系统；Theme 只设了默认字号与描边。
- 不做 CJK 字体与本地化（当前 UI 文案一律英文，见风险）。
- 不实现交互意图路由：hint / select / confirm / context 只在本节「交互意图抽象（设计，未实现）」做设计，本轮元素仍直接处理设备事件。
- 不在 release 构建暴露调试入口：Debug 区只在 `OS.is_debug_build()` 为真时出现，也不做隐藏手势。

## 待办与风险

| 项 | 影响 | 处理 |
| --- | --- | --- |
| 无 CJK 字体资源 | 中文文案显示方块，UI 只能用英文 | 引入字体（`.ttf` 或 `SystemFont`）并挂到 `UiRoot` 的 Theme `default_font` 后再做本地化；未做 |
| Android 真机安全区未验证 | headless 下 `safe_insets` 恒 0，`get_display_safe_area()` 在部分 ROM 上返回值不可靠 | 需要真机跑一遍刘海 / 挖孔 / 手势条；未验证 |
| 真机触摸 / 多指未验证 | 摇杆、视角区、自绘 ActionButton / PauseButton / TouchSlider 只在 headless 里用构造的 `InputEventScreenTouch` 断言过；真实触摸事件分发、同时三指以上、系统手势冲突、`canceled=true`、滑条拖动手感未验证 | 真机回归；必要时补集成测试 |
| 移动端入口只在 debug 构建整包验证过 | Debug 区按 `OS.is_debug_build()` 显示，headless 是 editor 构建（恒 true），release 包的实际隐藏未导包验证；触屏暂停按钮 / 触摸滑条的真机命中与手感也未验证 | 出一版 debug / release APK 各点一遍；未验证 |
| Button 触摸与多点触控结论 | verifier 实测 Godot 4.7.2 原生 Button 支持 ScreenTouch 与多指（两指各按一个独立、同一按钮第二根手指忽略、drag 出矩形后 release 按 index 回投）；自绘 `ActionButton` 是释放时机 / per-index / HudWidget 契约的可选增强，不是绕行 | 已实证（headless / verifier）；真机细节见「真机触摸 / 多指未验证」 |
| P4 只有规则没有界面 | `group_id` 只实现注册表互斥 + 冒烟测试，没有具体背包 / 面板界面落地 | 随实际界面一起验收 |
| 交互意图抽象未实现 | 只有本文「交互意图抽象（设计，未实现）」一节；元素仍然直接处理设备事件 | 非本轮范围，后续期次落地 |
| 移动端 renderer 未定 | `project.godot` 仍是 Forward Plus；移动端要不要切 mobile renderer 没定 | 与 UI 适配解耦，但影响安卓帧率；待性能测试 |
| mod UI 加载器未做 | 只有 `register_external` API，没有 manifest 驱动的发现 / 排序 / 卸载 | 随 Mod M6 一起设计；见 [[08-静态数据与Mod体系]] |
| 手柄只做到方案检测 | 暂停菜单内部有 `grab_focus` 可键盘 / 手柄操作，但局内 HUD 与交互没有焦点链、也没有按键提示 | 需要局内焦点导航与手柄提示；未做 |
| 触屏手感未调 | 摇杆死区 0.15、底圈半径 140、按钮 96×96、视角灵敏度 1.0 都是初值 | 真机调参；未做 |
| 暂停菜单 Quit 无确认 | 误触直接 `get_tree().quit()` | 加二次确认或主菜单流程；未做 |
| 方案状态是单值覆盖 | `scheme` 是位掩码但当前只记一个来源，手柄 + 键鼠同时接入会互相覆盖 | P5 已加切换冷却与幅度阈值做防抖，但仍是「最近事件获胜」，没有多源合并；未做 |

已解除 / 已接受（2026-10-22）：

- **交互提示缺 ActionId 反查已解决（P3b）**：键鼠提示改为从 InputMap 该动作绑定的第一个键鼠事件取键名（键盘 `OS.get_keycode_string(physical_keycode)`、鼠标 `as_text()`），触屏显示 `Tap`；不再硬编码「按 E」。一个动作多绑定时只展示第一个键鼠事件。

## 维护约定

- 架构变化时改对应笔记，并在 [[11-当前状态与风险]] 更新状态表。
- 新增输入通道 / 按钮规格时，同步改 `ButtonSpecs` 与本笔记的「动作按钮」一节。
- 改 `scheme` / `platform` / `compact_level` 的语义时，同步改「三量分离」与测试矩阵。
