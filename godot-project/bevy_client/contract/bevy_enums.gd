class_name BevyEnums
## godot-client-ext 表现/输入原语的语义常量。
##
## 与 Rust 侧必须保持一致：
## - Cmd 对应 game_core::presentation::packed::kind
## - Payload 对应 game_core::presentation::payload::PayloadKind（sync_payloads! 是
##   唯一真值源；packed::tests::godot_payload_enum_matches_rust_registry 会校验）
## - Channel 是 BevyBridge 选择的表现消费路径
##
## 本文件是 Rust <-> Godot 的共享契约，不依赖 bevy_client 的任何其它脚本。

enum Channel {
	DICTIONARY,        ## take_presentation_frame() -> Dictionary
	PACKED_DICTIONARY, ## GPF1 -> parse_packed_presentation_frame()
	PACKED_STREAMS,    ## GPF1 -> take_packed_streams()（默认，最快）
}

## 节点被回收的原因。detach 与 despawn 语义完全不同。
enum RemovalReason {
	LEFT_VIEW, ## 离开 AOI / 本地视野：逻辑实体仍然存在，节点回收
	DESPAWNED, ## 实体真的消失：永久移除
}

## 命令 kind（game_core::presentation::packed::kind）。
enum Cmd {
	ATTACH = 0,
	ADD = 1,
	UPDATE = 2,
	REMOVE = 3,
	DETACH = 4,
	DESPAWN = 5,
}

## 载荷 kind（game_core::presentation::payload::PayloadKind::code()）。
## code 只能末尾追加，禁止重排/复用。
enum Payload {
	TRANSFORM = 0,
	PRESENTATION = 1,
	HEALTH = 2,
	VISIBILITY = 3,
	INTERACTION = 4,
	EXT = 5,
	PROTOTYPE = 6,
	RECTLIST = 7, ## 体素矩形实例流（只追加，禁止重排/复用）
}

## attach / detach / despawn 命令的 payload_kinds 哨兵（packed::PAYLOAD_NONE）。
const PAYLOAD_NONE := 0xFF
## 枚举条目数（Rust PayloadKind::ALL.len()），不进 enum 以免破坏契约测试。
const PAYLOAD_COUNT := 8

## extension 载荷字段 tag（与 game-core payload/packed.rs 对齐）。
const EXT_TAG_I32 := 0
const EXT_TAG_BOOL := 1
const EXT_TAG_TAGS := 2

## EXT 组件值 Dictionary 的键（bag 本体）。
const EXT_FIELD := "fields"
## PROTOTYPE 组件值 Dictionary 的键（静态原型 ID）。
const PROTOTYPE_FIELD := "prototype_id"

## 线格式 token：描述一个载荷在 SoA 池里的读取形状（PayloadCodec 用）。
enum Wire {
	F32,     ## f32 池：1 个 float
	ANGLE,   ## f32 池：1 个角度（插值时用 lerp_angle）
	VEC3,    ## f32 池：3 个 float -> Vector3
	I32,     ## i32 池：1 个 int
	BOOL,    ## i32 池：1 个 int，!= 0 即 true
	TAGS,    ## i32 池：n + n 个 int -> PackedInt32Array
	WORDS,   ## i32 池：n + n 个 int -> PackedInt64Array（保留 64 位；RECTLIST 用）
	EXT_BAG, ## i32 池：变长扩展袋（吃掉本条命令剩余槽位）
}

## 视图呈现方式：PayloadCodec 只产数据，ViewLayer 按它决定怎么演。
enum View {
	NONE,      ## 纯数据载荷，不接触节点
	TRANSFORM, ## 位姿：codec.sample_position / sample_yaw
	VISIBLE,   ## 可见性：node.visible
	MESH,      ## 体素矩形：node.set_mesh_view(lod, words)
}

## 载荷 schema。新增核心载荷 = 这里加一行（+ Rust 侧）。
## - fields：EntityData 组件值 Dictionary 的键，与 wire 逐项对应；
##           interp 载荷可只列暴露字段，未列出的尾部 wire token 作为纠正偏移。
## - wire  ：SoA 池读取形状。
## - view  ：ViewLayer 的呈现方式。
## - interp / stride / samples：仅插值载荷（TRANSFORM）需要。
const PAYLOAD_SCHEMA := {
	Payload.TRANSFORM: {
		"fields": ["position", "yaw"],
		"wire": [Wire.VEC3, Wire.ANGLE, Wire.VEC3],
		"view": View.TRANSFORM,
		"interp": true,
		"stride": 7,
		"samples": 2,
	},
	Payload.PRESENTATION: {
		"fields": ["locomotion", "action", "tags"],
		"wire": [Wire.I32, Wire.I32, Wire.TAGS],
		"view": View.NONE,
	},
	Payload.HEALTH: {
		"fields": ["current", "max"],
		"wire": [Wire.F32, Wire.F32],
		"view": View.NONE,
	},
	Payload.VISIBILITY: {
		"fields": ["visible"],
		"wire": [Wire.BOOL],
		"view": View.VISIBLE,
	},
	Payload.INTERACTION: {
		"fields": ["action", "enabled"],
		"wire": [Wire.I32, Wire.BOOL],
		"view": View.NONE,
	},
	Payload.EXT: {
		"fields": [EXT_FIELD],
		"wire": [Wire.EXT_BAG],
		"view": View.NONE,
	},
	Payload.PROTOTYPE: {
		"fields": [PROTOTYPE_FIELD],
		"wire": [Wire.I32],
		"view": View.NONE,
	},
	## 体素矩形实例流：i32 池 = [lod, count, word...]（39 bit 矩形 × count）。
	## WORDS 保留完整 64 位（绝不用 TAGS 截断成 32 位）；view=MESH 交给
	## VoxelMeshNode 用 RenderingDevice 在 GPU 上展开（路径 B）。
	Payload.RECTLIST: {
		"fields": ["lod", "rects"],
		"wire": [Wire.I32, Wire.WORDS],
		"view": View.MESH,
	},
}
