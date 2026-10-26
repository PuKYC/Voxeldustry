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

一句话：**一套 UI 树 + platform / scheme / compact_level 三量分离 + 九个 HudSlot 区域层（铺满 SafeArea、互相叠放）+ 触控区域避让层 + HudWidget 契约（含 scheme_overrides），不做布局求解器。** 桌面、手机、平板、手柄共用同一棵树：platform 决定缩放目标 / 安全区 / 基础边距，scheme 决定显隐与重排，compact_level 决定隐藏哪些元素。输入侧分三层：物理输入 → Godot 绑定层（InputBindings，直连 / 按目标上下文解析）→ Rust 语义动作（attack / break_block / place_block / jump / interact / ...）。触屏动作簇是一个 HudWidget（ActionCluster，落 bottom_end），内部按弧线排按钮；键鼠 / 手柄不出按钮，只出键位提示，并在提交前按上下文重映射。Rust 只认语义动作与能力位（present / available），不认键位。

## 目标与边界

| 目标 | 做法 |
| --- | --- |
| 一套 UI 适配桌面 / 手机 / 平板 / 手柄 | 单 UI 树 + 输入方案位掩码显隐，不维护两套界面 |
| 分辨率与刘海屏自适应 | HudSlot 区域层 + 断点 + 移动端目标逻辑宽度 + 安全区内缩 + 触控区域避让 |
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
| `godot-project/ui/ui_root.gd` / `ui_root.tscn` | UI 根（CanvasLayer）：9 个 HudSlot 区域层、安全区、缩放、断点、platform / scheme 检测与防抖、触控区域避让层、菜单 / 暂停、期 3 设置接线、F3 调试层 | 已实现 |
| `godot-project/ui/hud_slot.gd` | 自绘区域层容器：最小尺寸只取 reserve、主轴对齐 / 自动换行、inset 避让、边缘预留带 | 本轮新增 |
| `godot-project/ui/touch_regions.gd` | 触控区域登记表：摇杆 / 动作簇 / 视角区的矩形与槽位 inset 汇总 | 本轮新增 |
| `godot-project/ui/ui_state.gd` | UI 全局静态状态，无 Autoload；期 3 设置字段 + `load_settings` / `save_settings`（user://ui_settings.cfg） | 已实现 |
| `godot-project/ui/hud_widget.gd` | HudWidget 契约基类（Control）；`scheme_overrides` 按方案覆盖 tray / align / priority / min_size | 已实现 |
| `godot-project/ui/hud_registry.gd` | 槽位注册表 + priority 排序（不再依赖 BoxContainer）+ `relocate()` + 同组互斥 + mod 注册校验 | 已实现 |
| `godot-project/ui/input/input_bindings.gd` | 输入绑定层（纯 static）：槽位 → 语义动作、上下文解析、预设、键位提示、KBM 提交前重映射 | 本轮新增 |
| `godot-project/ui/input/action_cluster.gd` | 触屏动作簇（HudWidget，`bottom_end`）：主按钮 + 弧形副按钮、物理尺寸、透明度、左手镜像、避让登记 | 本轮新增 |
| `godot-project/ui/input/action_button_host.gd` | 动作簇宿主：读语义通道表 / 能力位 / INTERACTION 目标，刷新动作簇与键鼠提示，统一释放 | 本轮新增 |
| `godot-project/ui/input/button_specs.gd` | 旧通道 → 按钮映射表，已废弃（保留空壳避免旧脚本解析失败） | 已废弃 |
| `godot-project/ui/widgets/action_button.gd` | 自绘动作按钮（`Control`，由 ActionCluster 管理），按触摸 index 产生按下 / 释放 | 本轮重写 |
| `godot-project/ui/widgets/action_hints.gd` | 键鼠 / 手柄动作提示（仅 KBM / GAMEPAD 显示） | 本轮新增 |
| `godot-project/ui/debug/hud_debug_overlay.gd` | F3 只读调试层（尺寸 / 缩放 / 安全区 / 断点 / 元素矩形 / 重叠告警） | 已实现 |
| `godot-project/ui/widgets/` | 内置 HUD：准星、血条、交互提示、触屏暂停按钮 | 已实现 |
| `godot-project/ui/widgets/touch_slider.gd` | 触摸可拖动的 `HSlider`（原生 HSlider 只认鼠标事件） | 已实现 |
| `godot-project/ui/widgets/pause_button.gd` / `.tscn` | 触屏暂停按钮（仅 TOUCH 方案显示，落 TOP / END 槽位） | 已实现 |
| `godot-project/ui/touch/touch_controls.gd` / `.tscn` | 触屏控件层：虚拟摇杆 + 视角区 + 浮动摇杆区；左手镜像、避让登记 | 已实现 |
| `godot-project/ui/touch/touch_joystick.gd` | 单指虚拟摇杆，写 `move_*` 动作强度；推到边缘自动 `sprint` | 已实现 |
| `godot-project/ui/screens/pause_menu.gd` / `.tscn` | 暂停菜单：Resume / Settings（UI 缩放 + 期 3 设置区）/ Quit；debug 构建额外显示 Debug 区 | 已实现 |
| `godot-project/bevy_client/camera/camera_rig.gd` | 相机：`camera_rig` 组、`set_ui_blocked()`、触屏 / 菜单不捕获鼠标 | 已实现 |
| `godot-project/test/ui_smoke_test.gd` | UI headless 冒烟（`extends SceneTree`），按 P0-P6 分阶段覆盖 | 已实现 |
| `godot-project/test/gen_class_cache.gd` | 无编辑器时生成 `class_name` 缓存 | 已实现 |
| `tools/ui_check.sh` | 一步跑完类名缓存 + UI 冒烟 | 已实现 |

`UiRoot` 的场景文件只挂脚本，整棵布局树在 `_build_layout()` 里用代码构建，避免场景文件与适配逻辑两头维护。

可选子场景按“存在才加载”处理（`ResourceLoader.exists`）：触屏控件、暂停菜单、内置 HUD 场景缺失时 UiRoot 照常启动，只是少对应部件。这让 UI 骨架与具体控件可以分开落地。`ActionButtonHost` 不是场景：`enable_touch_controls` 为真时由 UiRoot 在代码里创建，它再创建 `ActionCluster`（触屏）与 `ActionHints`（键鼠 / 手柄）；无 client 时按钮数为 0。

## 布局树

```text
UiRoot (CanvasLayer, layer=10, PROCESS_MODE_ALWAYS)
└ UiTree (Control, 全屏, MOUSE_FILTER_IGNORE, 内置 Theme)
  └ SafeArea (MarginContainer)          ← 安全区 + 边距只在这里加
    ├ TopBegin (HudSlot, TOP/BEGIN)     ┐
    ├ TopCenter (HudSlot, TOP/CENTER)   │ 9 个区域层全部铺满 SafeArea，
    ├ TopEnd (HudSlot, TOP/END)         │ 互相叠放、各自只对一个边缘负责。
    ├ MidBegin (HudSlot, LEFT/CENTER)   │ 最小尺寸只取 reserve，
    ├ MidCenter (HudSlot, CENTER)       │ 不取子元素之和。
    ├ MidEnd (HudSlot, RIGHT/CENTER)    │
    ├ BottomBegin (HudSlot, BOTTOM/BEGIN)│
    ├ BottomCenter (HudSlot, BOTTOM/CENTER, reserve 72 = 快捷栏预留带)
    └ BottomEnd (HudSlot, BOTTOM/END)   ┘
```

布局规则从「托盘行 + 竖列」改成「9 个独立叠放的区域层」：

| 问题（旧布局） | 新布局 |
| --- | --- |
| 槽位是 HBox / VBox，最小尺寸 = 子元素之和；元素一多就把 MidTray 撑出屏幕 | HudSlot 最小尺寸只取 reserve；子元素再多也只在自己的内容区里换行 / 夹取，不改变槽位尺寸 |
| 子元素少 / 隐藏时槽位塌成 0，上下托盘高度变化推动中间区域，准星漂移 | 每个槽位都铺满 SafeArea，彼此不共享高度；mid_center 恒等于安全区中心 |
| 元素增减导致其它元素位移 | 跨槽位完全独立；同槽位内的重排只影响该槽位 |

九个槽位全部 `MOUSE_FILTER_IGNORE`，不拦输入，真正可交互的控件自己打开鼠标过滤。

### HudSlot 行为

- 主轴：top / bottom 水平，mid 垂直；主轴放不下自动换行 / 换列（`wrap`），交叉轴由 `edge` 决定（TOP 贴顶、BOTTOM 贴底、LEFT 贴左、RIGHT 贴右、CENTER 居中）。
- 主轴对齐 `align`：BEGIN 从头排、END 贴端、CENTER 整体居中；换行 / 换列朝远离该边缘的方向展开。
- `inset: Vector4` 给槽位划避让区；`reserve: Vector2` 是边缘预留带（bottom_center 的 72 就是快捷栏占位，现在没有元素也保留）。
- 隐藏的子元素不占位、不参与排布（可见性变化会触发 deferred 重排）。
- 子元素请求尺寸取 `get_combined_minimum_size()` 并夹到内容区；若引擎最小尺寸拒绝缩小，记录 `debug_warnings`，只把分配 footprint 夹进内容区，保证后续元素不被它推出去。调试层与测试都按 footprint 判定越界 / 重叠。
- 槽位直接设置子元素 `position/size`，因此子元素必须保持默认 anchors。

### 避让层（TouchRegionRegistry）

触控区域不在 SafeArea 里，必须向区域层登记自己占用的矩形：

```text
VirtualJoystick / JoystickZone ─┐
ActionCluster ────────────────┼─→ UiRoot.update_touch_region(name, rect, reserves)
LookArea（只登记不避让）──────┘        └→ TouchRegionRegistry.slot_insets()
                                          └→ HudSlot.set_inset()
```

- `reserves` 形如 `{"bottom_begin": Vector4(left, top, right, bottom)}`，单位是设计单位；同一槽位每边取 max。
- 摇杆：右手登记 `bottom_begin` 左避让（宽度 + 边距），左手登记 `bottom_end` 右避让；浮动摇杆未激活时释放登记。
- 动作簇：登记自身矩形（`reserves` 为空，仅用于重叠诊断；簇自身就在槽位里，不能再给自己避让）。
- 视角区：只登记矩形不避让——HUD 在它上层且按钮先吃触摸。
- 平板触屏额外把底部槽位往里收（`TABLET_EXTRA_EDGE`），让动作簇离边缘更远。

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
| LEFT | 忽略 | `mid_begin` |
| RIGHT | 忽略 | `mid_end` |
| CENTER | 忽略 | `mid_center` |

LEFT / RIGHT 的 Align 不改变落点，只影响同槽内的排序。

槽内顺序由 `HudRegistry._insert` 在插入时按 `effective_priority()` 算 index（任意 Control 槽位都适用，不再要求 BoxContainer）：BEGIN 槽里 priority 越大越靠屏幕边缘，其余槽相反。

注册规则（`HudRegistry.register`）：

1. `widget_id` 为空时退回节点名；为空且无节点名直接拒绝。
2. 重复 id 拒绝，不静默覆盖。
3. `tray` 取 `CENTER` 或未知值时都落 `mid_center`；只有注册表还没 `setup()`（槽位不存在）时才因取不到槽位而拒绝。
4. 插入槽位后调 `attach(ui_root)`，再按当前方案 `relocate()`（槽位 / 排序 / min_size）并重算同组激活，最后广播 `scheme` / `compact_level` / 菜单状态；随后对 HudSlot 调 `mark_dirty()`。`refresh_scheme()` / `refresh_compact()` / `refresh_menu()` 同样在末尾重排。

**角色 → 区域映射**（本期只落状态条与动作按钮）：血条键鼠 / 手柄落 `bottom_begin`、触屏用 `scheme_overrides` 落 `top_begin`；暂停按钮仅触屏、落 `top_end`；动作簇仅触屏、落 `bottom_end`（左手模式改 `bottom_begin`）。

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
5. 触屏控件挂在 `SafeArea` 之外（贴屏幕物理边缘），所以它的 layout 自己读 `UiState.safe_insets` 避让：摇杆避开左 / 下 insets（视角区是拖拽区、不承载按钮）。触控区域再通过 `update_touch_region()` 向避让层登记矩形，给 `bottom_begin` / `bottom_end` 设置 inset；动作簇作为 HudWidget 进 `bottom_end`，安全区由 `SafeArea` 统一处理。安全区更新后 UiRoot 调 `touch_controls.refresh_layout()` 重排一次；移动端 insets 要延迟一帧才准，所以重排不能省。

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
| `action_layout` | int | CONTEXT | 操作布局预设：CONTEXT（精简）/ SPLIT（分开） |
| `button_scale` | float | 1.0 | 动作按钮大小系数 0.8–1.3 |
| `button_opacity` | float | 0.75 | 动作按钮空闲透明度 0.35–1.0 |
| `left_handed` | bool | false | 左手模式：动作簇 / 摇杆 / 视角区镜像 |
| `joystick_mode` | int | FIXED | 摇杆模式：FIXED / FLOATING（左半屏按下处浮起） |

写方只有 UiRoot（`platform` 在 `_apply_scale()` 开头写，`scheme` 在方案检测 / 切换时写）；读方是 HUD 元素、CameraRig、BevyClient。辅助方法：

| 方法 | 用途 |
| --- | --- |
| `has_scheme(v)` | 方案位命中 |
| `set_scheme(v)` | 写方案，返回“是否真的变了”供调用方决定刷新 |
| `is_menu_open()` / `set_menu_open(b)` | 菜单深度 |
| `accepts_gameplay_input()` | `menu_depth == 0`，BevyClient 每帧据此决定是否提交输入 |
| `mouse_look_allowed()` | 菜单未开且非触屏方案 |
| `physical_units(mm)` / `action_button_units(role)` | 毫米 → 设计单位（DPI / content_scale_factor 换算，带下限与回退） |
| `load_settings()` / `save_settings()` | 读写 `user://ui_settings.cfg`（`SETTINGS_VERSION=1`） |
| `reset()` | 全部归默认（测试 / 退出场景用；不删设置文件） |

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

触屏不新增 Rust 输入链路：摇杆 / 动作簇把触摸翻译成已有的语义动作，视角走 `BevyInput.on_look_delta`，Rust 侧仍是同一条 held / pressed / released + move + look 链路。

```text
InputEventScreenTouch / ScreenDrag
  → 触屏控件层（ui/touch/）
     ├ 摇杆：Input.action_press("move_*", strength) / release；推到边缘自动 sprint
     └ 视角区：ScreenDrag.relative → BevyInput.on_look_delta()
  → 动作簇（ActionCluster + ActionButton）
     绑定层解析出的语义动作 → Input.action_press / action_release
  → BevyInput.collect_and_submit()：held / pressed / released + get_vector + look_delta
  → submit_input_frame() → InputStaging → 量化 / 能力过滤 / 事件派生（Rust 不变）
```

| 触摸控件 | Godot 落点 | Rust 落点 |
| --- | --- | --- |
| 虚拟摇杆 | `move_forward/back/left/right` 动作强度；边缘额外 `sprint` | `get_vector` 合成移动轴 → `quantize_axis`；sprint 走通道位 |
| 视角区 | `ScreenDrag.relative × look_sensitivity` → `on_look_delta` | 与鼠标同一个 `look_delta` → `quantize_look`，不占动作位 |
| 动作簇 | 绑定层解析出的 `attack / break_block / place_block / jump / interact / glide / flight_toggle` | 进 held / pressed / released，与键盘手柄一致 |

关键约定：

- `emulate_mouse_from_touch=false`；采集层忽略 `DEVICE_ID_EMULATION` 的合成鼠标事件。
- 多指按 `event.index` 路由：摇杆只跟第一根手指，视角区用 index 集合，ActionButton 每个按钮跟一根手指；动作簇在 HUD 上层（触摸控件层被 `move_child(..., 0)` 压到最底），按钮先吃触摸，其余滑动才落到视角区。
- 隐藏 / 切走触屏方案 / 打开菜单 / 失焦 / 退出场景树时统一释放：触屏层复位摇杆与视角手势，`ActionButtonHost.release_all()` 释放按钮动作。
- 摇杆 / 动作簇 / 视角区在布局变化时向 `UiRoot.update_touch_region()` 登记矩形（见「避让层」）。

## 输入绑定层与动作簇

### 三层分工

```text
物理输入（键盘 / 触屏 / 手柄）
  → Godot InputBindings（绑定层，可配置）
      物理槽位 → 语义动作（直连 / 上下文解析）
  → Rust 语义动作 + 能力（present / available，确定性不变）
  → INPUTCAPS 回传
  → UI 按钮 / 键位提示 / 显隐（全部由语义能力生成）
```

Rust 只认语义动作与能力，不知道键位；绑定解析在提交前完成，预测回滚与 `InputBuffer` 重演不受影响。

### 语义动作表（Rust）

| 动作 | 通道 | 能力组件 | 说明 |
| --- | --- | --- | --- |
| `attack` | `attack` | `AttackChannel` | 攻击 / 主操作 |
| `break_block` | `break_block` | `BreakBlockChannel` | 破坏方块 |
| `place_block` | `place_block` | `PlaceBlockChannel` | 放置方块 |
| `jump` / `glide` / `flight_toggle` / `interact` / `sprint` | 同名 | 同名 Channel | 已是语义命名 |

`tool` 通道与 `primary_tool` / `secondary_tool` 已删除。眩晕（`Stunned`）削减 move / jump / attack / break_block / place_block 的 `available`，但 `present` 不变，因此按钮只置灰、不消失、不位移；interact 不受眩晕影响。动作位布局随动作表变化（attack bit8、break_block bit9、place_block bit14），`INPUTCAPS` 字段 `present` / `available` 形状不变，只是位域内容变了。

`INTERACTION` 载荷新增 `target: u8`（0=none / 1=entity / 2=block），供绑定层在 context 预设下决定主操作解析成 attack 还是 break_block。

### InputBindings：槽位 → 语义动作

一个物理槽位有两种绑定：

- **直连**：槽位 → 一个语义动作（split 预设：attack / break_block / place_block 各一个按钮）。
- **上下文**：槽位 → 有序候选列表，取第一个 present 且 available 的动作；都不可用时取第一个 present 的并置灰（不位移）。context 主槽位候选 `[attack, break_block]`，target=block 时反转为 `[break_block, attack]`。

| 预设 | 触屏右下角 | 键鼠 / 手柄 |
| --- | --- | --- |
| 精简 Context（默认） | 主按钮（attack / break_block 上下文）、副按钮（place_block）、跳跃；Interact 仅在有可交互目标时出现 | 无按钮；左键主、右键副、中键 break，提交前按 target 重映射 |
| 分开 Split | attack、break_block、place_block、跳跃各一个按钮 | 三个动作各绑一个键，不做上下文重映射 |

槽位显隐规则：任一候选动作的通道 `present` 才显示；全无则隐藏；Interact 额外要求 `INTERACTION.enabled`。按钮文案取当前解析出的动作，目标变化时跟着变。`glide` / `flight` 仅在能力 present 时占位；sprint 不再出按钮，由摇杆推到边缘自动触发。

键鼠 / 手柄提示（`ActionHints`）由 `InputBindings.hint_text()` 从 InputMap 真实绑定生成（例：`[LMB] Attack  [RMB] Place  [Space] Jump`），触屏返回空串、整行隐藏。

### ActionCluster：触屏右下角动作簇

`ActionCluster` 是**一个** `HudWidget`（`widget_id=action_cluster`、`tray=BOTTOM`、`align=END`、`priority=10`、`schemes=TOUCH`），内部管理 `ActionButton` 子控件（按钮不再单独注册进 HudRegistry）。

- **位置**：只落右下角；右上角只有暂停按钮；右侧中部留空给拇指滑动视角。
- **主按钮最大**：主按钮（primary / 左手时的首个锚点）约 10 mm，其余约 9 mm，跳跃 9.5 mm；由 `DisplayServer.screen_get_dpi()` 换算 → 设计单位（除以 `content_scale_factor`），并乘期 3 的按钮大小设置。平板 ×1.1。
- **弧形**：主按钮钉在簇的右下角（左手为左下角），其余按钮按 `[180°, 215°, 250°, 285°, 320°]` 极坐标绕主按钮展开。因为簇在 `bottom_end` 是 END / BOTTOM 对齐，簇向右下固定：Interact 等动态按钮出现时簇向左 / 上长大，**主按钮位置不跳**。
- **常驻上限 3–4 个**：context 3 个（主 / 副 / 跳）+ 条件 Interact；split 4 个。glide / flight 只在能力 present 时占位；其余不出现。
- **触摸优先级**：按钮 `mouse_filter=STOP`，吃掉落在自己矩形内的触摸；其余滑动走下层视角区。
- **透明度**：空闲 alpha = 期 3 的按钮透明度（默认 0.75），按下变亮。
- **左手模式**：`align=BEGIN` + 内部水平镜像；摇杆镜像到右下，视角区镜像到左侧。
- **避让**：按自身 global rect 调 `update_touch_region("action_cluster", ...)`（reserves 为空），供调试层做重叠告警；动态显隐 / resized 时更新。

`ActionButton` 是自绘 `Control`：按下 `Input.action_press(action)`、抬起 `action_release`，toggle 模式点按后 0.1s 自动释放；不可用 / 隐藏 / 失焦 / 退出树都会立即 release（防粘键）。一个按钮只跟一根手指。

### 期 3 设置（落盘）

`UiState` 新增 5 个字段并持久化到 `user://ui_settings.cfg`（`ConfigFile`，section `ui`，`SETTINGS_VERSION=1`）：

| 设置 | 字段 | 取值 | 生效点 |
| --- | --- | --- | --- |
| 操作布局 | `action_layout` | CONTEXT / SPLIT | 绑定层重解析 → 动作簇按钮数量 / 文案 |
| 按钮大小 | `button_scale` | 0.8–1.3 | 动作簇按钮边长 |
| 按钮透明度 | `button_opacity` | 0.35–1.0 | ActionButton 空闲 alpha |
| 左手模式 | `left_handed` | bool | 动作簇镜像 / 摇杆与视角区镜像 |
| 摇杆模式 | `joystick_mode` | FIXED / FLOATING | 固定摇杆 vs 左半屏按下处浮起 |

暂停菜单 Settings 区新增对应控件（运行期创建，插在 Back 前），交互只发信号；`UiRoot` 统一写 `UiState` + `save_settings()`，再调 `action_host.refresh_settings()` / `touch_controls.refresh_settings()` 立即生效。`UiRoot.load_persisted_settings` 仅供测试关闭读取。

### 快捷栏预留（本期不实现）

`bottom_center` 槽位 `reserve=(0,72)`：现在没有快捷栏元素也保留这条底部带，其它元素不会挤进去。动作簇总宽度受常驻上限约束，中间留给未来的快捷栏。期 4 再实现快捷栏本体。

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

1. **现状（绑定层已落地）**：`ActionButton` 是自绘 `Control`（由 `ActionCluster` 管理，不再单独注册），自己按 `InputEventScreenTouch.index` 处理触摸；`ActionButtonHost` + `InputBindings` 负责语义动作解析、上下文显隐与释放。这不是因为「原生 Button 不支持触摸」——4.7.2 上原生 Button 支持触摸与多指，自绘是为了释放时机、per-index 语义与动作簇契约一致。
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
| 期 3 设置 | 操作布局（Context/Split）、按钮大小（0.8–1.3）、按钮透明度（0.35–1.0）、左手模式、摇杆模式；运行期创建在 Back 之前，只发信号，由 UiRoot 写 `UiState` + `save_settings()` 并立即生效 |
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
| `HealthBar` | 默认 `tray=BOTTOM, align=BEGIN, priority=20`；TOUCH 覆盖为 `TOP/BEGIN` | 260×30 | `listen_component(HEALTH)` + `controlled_entity_changed`；读 `current` / `max` 填 ProgressBar 与数值 Label |
| `ActionHints` | `tray=BOTTOM, align=BEGIN, priority=10`，`schemes=KBM|GAMEPAD` | 文本行 | 由 `ActionButtonHost` 创建；`InputBindings.hint_text()` 从 InputMap 生成键位提示，空串隐藏 |
| `ActionCluster` | `tray=BOTTOM, align=END, priority=10`，`schemes=TOUCH` | 动态 | 由 `ActionButtonHost` 创建；语义动作解析 + 弧形按钮 + 避让登记 |
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

| 脚本 | 覆盖 |
| --- | --- |
| `tools/ui_check.sh` | 生成类名缓存 + `.godot/extension_list.cfg`，依次跑下面 4 个脚本，任一失败非零退出；`--gameplay` 追加真实场景探针 |
| `test/ui_smoke_test.gd` | 9 个 HudSlot 槽位 / 准星居中 / 隐藏元素不位移 / 内置 HUD / 方案与紧凑 / 菜单 / mod 路径 / 触屏控件 / 期 3 设置接线 / 动作簇与绑定层（context 3 按钮、target 切换、Interact 条件出现、split 4 按钮、置灰不位移、左手） |
| `test/ui_layout_test.gd` | SubViewport 844×390 / 1280×800 / 1920×1080：footprint 不越界、mid_center 居中、换行换列、reserve 72、inset 避让、隐藏不变、TouchRegionRegistry |
| `test/input_bindings_test.gd` | 绑定层 context / split 解析、target 切换、interact 显隐、stun 置灰、动作簇弧形 / 锚点稳定 / 左手镜像、ActionButton 多点触控与释放、键位提示 |
| `test/ui_settings_test.gd` | 单位换算、落盘恢复、坏文件回默认、暂停菜单 5 类设置控件与信号、左手镜像、浮动摇杆、血条角色映射、sprint 边沿 |

当前结果（2026-10-23，Godot 4.7.2 headless，aarch64）：

```text
[ui_smoke] checks=146 failures=0            UI_SMOKE PASS
[ui_layout] checks=41 failures=0            UI_LAYOUT PASS
[input_bindings] checks=114 failures=0      INPUT_BINDINGS PASS
[ui_settings] checks=122 failures=0         UI_SETTINGS PASS
```

布局验收口径（与方案 §9 对齐）：

- 没有元素 footprint 超出屏幕 / 安全区（多视口 headless + SubViewport）。
- 准星中心 == 安全区中心（误差 < 0.01）。
- 隐藏任一元素后，其它槽位元素位置不变（同槽位内按可见元素重排，见「HudSlot 行为」）。
- 动作簇 / 摇杆 / 暂停按钮 / 快捷栏预留带互不重叠。
- 真机复测仍是必须项：headless 的 DPI / 安全区 / 触摸分发都是 stub。

人工矩阵（每条都要过一遍）：

| 环境 | 期望 |
| --- | --- |
| 1280×720 / 1920×1080 键鼠 | 断点正常，HUD 贴边不越界，键位提示可见、动作簇不可见 |
| 手机横屏（逻辑宽 ~900） | compact_level=1，触屏动作簇右下、摇杆左下、暂停右上、准星居中 |
| 平板（逻辑宽 ~1280，触屏） | 断点 0，动作簇离边缘更远、按钮略大 |
| Android 刘海 / 手势条 | HUD 不压安全区（**未真机验证**） |
| Android 真机多指 | 按钮 + 摇杆 + 视角三指同按各自独立；系统取消触摸（`canceled=true`）释放动作（**未真机验证**） |
| 左手模式 / 浮动摇杆 / 按钮大小 / 透明度 | 切换后立即生效，重启保持（**手感未真机验证**） |

## 不做什么

- 不做布局求解器 / flex / 约束求解；HudSlot + 槽位就是全部布局能力。
- 不做 desktop 与 mobile 两套 UI 树。
- 不在 Rust 侧区分触屏输入源；触屏只是语义动作的另一个来源。
- 不在 HudWidget 子类里写坐标、分辨率判断、安全区计算。
- 不做 UI 动画、转场、UI 音效系统。
- 不做 CJK 字体与本地化（UI 文案一律英文）。
- 不实现交互意图路由：hint / select / confirm / context 仍只是设计。
- 期 4 快捷栏、期 5 拖动自定义布局不在本轮；只保留 `bottom_center` 的 72 预留带。
- 不在 release 构建暴露调试入口。

## 待办与风险

| 项 | 影响 | 处理 |
| --- | --- | --- |
| 无 CJK 字体资源 | 中文文案显示方块，UI 只能用英文 | 引入字体并挂 Theme 后再本地化；未做 |
| 真机安全区 / DPI 未验证 | headless 的 `safe_insets` / `screen_get_dpi` 是 stub，物理尺寸用回退 160 dpi | 真机回归；`physical_units` 已有下限与回退 |
| 真机触摸 / 多指未验证 | 摇杆、动作簇、暂停按钮、浮动摇杆只在 headless 用构造事件断言 | 真机回归 |
| 动作簇弧形手感未调 | 角度 / 半径 / 9–10 mm 都是初值 | 真机调参 |
| 同槽位隐藏元素会重排 | 隐藏子元素不占位，同槽位其余元素会前移（跨槽位不受影响） | 若产品要求严格不位移，可改为「隐藏也占位」；当前按「跨槽位不变」验收 |
| 单元素超尺寸被夹 | Godot 的 `size` 会被最小尺寸钳住，只能夹分配 footprint | `debug_warnings` 记录；跨槽位不受影响 |
| 破坏 / 放置已接最小闭环 | `gameplay::block_edit` 消费 `ResolvedInput` 的 break/place 边沿，每 tick 写 `InteractionHint.target`；CONTEXT 预设下主按钮已能按 target 切 Break | 真机验证按钮显隐 / 置灰与手感；粗 LOD 不跟随编辑（见 [[11-当前状态与风险]]） |
| 移动端 renderer 未定 | 仍是 Forward Plus | 与 UI 解耦，待性能测试 |
| mod UI 加载器未做 | 只有 `register_external` API | 随 Mod M6 |
| 手柄只做到方案检测 | 局内 HUD 无焦点链；动作提示可用但未做手柄图标 | 后续 |
| 暂停菜单 Quit 无确认 | 误触直接退出 | 后续 |
| 方案状态是单值覆盖 | 多设备同时输入仍是最近事件获胜（P5 已防抖） | 后续 |

## 维护约定

- 架构变化时改对应笔记，并在 [[11-当前状态与风险]] 更新状态表。
- 新增语义动作时同步改：`game-core/src/input/actions.rs` 的 `ACTION_TABLE` → `capability.rs` 的通道 / 能力组件 → `InputBindings` 的槽位配置 / 文案；不再单独维护 ButtonSpecs。
- 改 `scheme` / `platform` / `compact_level` / `action_layout` 的语义时，同步改「三量分离」「输入绑定层与动作簇」与测试矩阵。
- 调 HudSlot 布局规则时，必须同步跑 `ui_layout_test.gd`（多视口）。
