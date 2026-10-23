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
	## 7 是已删除的 RECTLIST 遗位：有意留空，禁止复用。
	RAWVOXELS = 8, ## 原始体素 halo：Godot 调 gdext mesher 现算 39 bit 矩形流
	RAWVOXELGROUPS = 9, ## 同一 LOD 的一组分块：每块 34³ halo，各一个子 VoxelMeshNode
	CAMERA = 10,   ## 相机视角（弧度）：yaw 绕 +Y、pitch 绕 +X；由 CameraRig 消费
	## 输入能力：present = 能力组件并集，available = 经禁用削减后的可用位，
	## source = ControlledBy.source（0 = 无控制源），local = 是否挂 LocalPlayer。
	## view = NONE，由 BevyInputContext 消费并翻译成通道级信号。
	INPUTCAPS = 11,
}

## attach / detach / despawn 命令的 payload_kinds 哨兵（packed::PAYLOAD_NONE）。
const PAYLOAD_NONE := 0xFF
## 枚举条目数（Rust PayloadKind::ALL.len()），不进 enum 以免破坏契约测试。
const PAYLOAD_COUNT := 11

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
	EXT_BAG, ## i32 池：变长扩展袋（吃掉本条命令剩余槽位）
	BYTES,   ## i32 池：[byte_len, chunk_count, chunk...] -> PackedByteArray（RAWVOXELS 用）
	CHUNK_BLOCKS, ## i32 池：[count, (x, y, z, byte_len, skirt_mask, chunk_count, chunk...)*] -> Array[Dictionary]（RAWVOXELGROUPS 用；每块带 skirt_mask）
}

## 视图呈现方式：PayloadCodec 只产数据，ViewLayer 按它决定怎么演。
enum View {
	NONE,      ## 纯数据载荷，不接触节点
	TRANSFORM, ## 位姿：codec.sample_position / sample_yaw
	VISIBLE,   ## 可见性：node.visible
	RAWVOXEL,  ## 原始体素 halo：gdext mesher 现算后 node.set_raw_voxel_view(lod, blocks, skirt_mask)
	RAWVOXELGROUP, ## 体素分组：每块一个子 VoxelMeshNode，node.set_raw_voxel_group_view(lod, chunks)
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
	## 交互提示：i32 池 = [action, enabled, target]；target 0=none / 1=entity / 2=block
	## （与 Rust 的 INTERACTION_TARGET_* 一致），供绑定层按上下文解析主操作。
	Payload.INTERACTION: {
		"fields": ["action", "enabled", "target"],
		"wire": [Wire.I32, Wire.BOOL, Wire.I32],
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
	## 原始体素 halo：i32 池 = [lod, skirt_mask, byte_len, chunk_count, chunk...]；
	## BYTES 解成 PackedByteArray；skirt_mask 是 6 bit 跨 LOD 接缝裙边掩码
	## （ExternalPlane 顺序 [+x,-x,+y,-y,+z,-z]，0 = 无裙边）。view=RAWVOXEL 交给
	## VoxelMeshNode 调 gdext mesher 现算 39 bit 矩形流后，再用 RenderingDevice 展开。
	Payload.RAWVOXELS: {
		"fields": ["lod", "skirt_mask", "blocks"],
		"wire": [Wire.I32, Wire.I32, Wire.BYTES],
		"view": View.RAWVOXEL,
	},
	## 体素分组：i32 池 = [lod, count, (x, y, z, byte_len, skirt_mask, chunk_count, chunk...)*]；
	## CHUNK_BLOCKS 解成 Array[{x, y, z, skirt_mask, blocks}]（每块带 6 bit 跨 LOD
	## 裙边掩码，0 = 无裙边），view=RAWVOXELGROUP 交给 VoxelGroupNode：每块调 gdext
	## mesher，各由一个按 key 复用的子 VoxelMeshNode 绘制。fields/wire 的顶层形状
	## 不变（mask 在 CHUNK_BLOCKS 的逐块记录里，不是顶层字段）。
	Payload.RAWVOXELGROUPS: {
		"fields": ["lod", "chunks"],
		"wire": [Wire.I32, Wire.CHUNK_BLOCKS],
		"view": View.RAWVOXELGROUP,
	},
	## 相机视角：不对应任何显示节点（view = NONE），由 CameraRig 直接读 EntityData 里的组件值。
	## 约定：Godot 侧 rotation = Vector3(pitch, yaw, 0)，位置走同实体的 TRANSFORM 载荷。
	Payload.CAMERA: {
		"fields": ["yaw", "pitch"],
		"wire": [Wire.ANGLE, Wire.ANGLE],
		"view": View.NONE,
	},
	## 输入能力：i32 池 = [present, available, source, local, look]（u64 掩码按位装进
	## i64，只用低位；local / look 用 0/1）。view = NONE，由 BevyInputContext 订阅
	## 并翻译成 controlled_changed / capabilities_changed / look_changed / channel_changed。
	Payload.INPUTCAPS: {
		"fields": ["present", "available", "source", "local", "look"],
		"wire": [Wire.I32, Wire.I32, Wire.I32, Wire.BOOL, Wire.BOOL],
		"view": View.NONE,
	},
}
