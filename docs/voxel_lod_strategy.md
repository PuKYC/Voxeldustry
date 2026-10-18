# 体素 LOD 策略（当前实现）

> 状态（2026-10-18）：A / B / C 三阶段已落地，旧的 A–F2 阶段计划已过时。
>
> - **A**（a96c5a8）：LOD 块由 `WorldGenerator::generate_block_at_lod` 按自身
>   LOD 直接生成，不再把 LOD0 子块折叠成粗格。
> - **B**（e69349b）：块只看**同 LOD** 邻居，缺失按空气处理 -> 每块自己封边；
>   删除跨 LOD 面剔除与跨 LOD halo 采样。
> - **C**（本文档同批提交）：`append_skirts` 改为 godot_voxel 风格的块内判据，
>   `skirt_mask` 扩为「跨 LOD 错位面 | 面外没有同 LOD 邻居的面」。自闭合后
>   这些面的面外 halo 是空气，当前 extract 路径下裙边是 **no-op**（见 §6.3），
>   保留为正确性安全网 + 切向轴修复。
>
> 硬约束：LOD0 输出逐位不变；`skirt_mask == 0` 输出逐位不变；wire 格式
> （34³ raw + 1 字节 mask）、FFI、GDScript 一律不动。

## 1. 三层设计（先看这个）

1. **按自身 LOD 生成**：每个 `TerrainBlock` 都有自己的 `lod`。LOD>0 的块由
   `generate_block_at_lod` 直接按 32³ 粗格生成（每格 `2^lod` 个基础体素），
   不读、不折叠 LOD0 数据；LOD0 仍从已生成的 `VoxVolume` 包装，逐位不变。
2. **同 LOD 邻接 + 自闭合**：`face_neighbors` 把 `Coarser` / `Finer` 一律
   映射成 `Missing`（按空气处理）。块只在同 LOD 邻居之间剔除面，因此跨 LOD
   交界处两侧各画各的边界墙面，不会出现「两侧都剔除」的穿透洞。
   **接缝现在是台阶墙、不是洞**；要压台阶得靠 LOD fade
   （`godot-project/bevy_client/entity/voxel_mesh.gd` 的 `lod_fade_duration`，
   与 `lod_skirts_enabled` 相邻，默认 0 = 关闭）或各向异性 LOD，不是靠裙子。
3. **接缝裙边（安全网）**：`skirt_mask` 在「跨 LOD 错位 / 面外没有同 LOD
   邻居」的面置位；mesher 在这些面上按块内判据补 1x1 裙边 rect（AO 满亮），
   并用覆盖位图跳过主 mesher 已经画过的格，避免共面重复 / z-fighting。
   自闭合后这些面的面外 halo 是空气，主 mesher 已经覆盖边界格，所以当前
   extract 路径下裙边是 **no-op**（见 §6.3）；它保留为正确性安全网 + 切向轴
   回归测试，不代表接缝靠它来遮。

## 2. godot_voxel 对照

源码已 clone 到 `/tmp/godot_voxel`（depth 1）。LOD 相关：

| 机制 | 源码 | 要点 |
| --- | --- | --- |
| LOD 八叉树 | `terrain/variable_lod/lod_octree.h` | 3D 八叉树；按观察者距离分裂/合并；每次 update 只推进一层；`is_below_split_distance` |
| 距离判定 | 同上 L196-201 | `lod_factor = 1<<lod`；`world_center = lod_factor*(pos+0.5)`；`split_distance = lod_distance*lod_factor` |
| 节点盒 | 同上 L188-191 | `get_node_box(pos,lod) = (pos<<lod, 1<<lod)`（单位 = 最深叶节点） |
| 平滑接缝 | `…update_task.cpp:700-792 get_transition_mask` | 每个面判断邻居更粗/更细 -> transvoxel transition mask（只用于平滑地形） |
| **方块接缝（对标物）** | `meshers/blocky/blocky_lod_skirts.h` | 只访问本块 voxel（含 padding 的 outer 层），给「outer 格实心、且切向暴露、且遮挡了 inner 格」的 inner 格补侧面几何；不做 AO；可按模型关闭；注释明说「不总能藏住所有裂缝」 |
| 抖动渐隐 | `voxel_lod_terrain.cpp` `_lod_fade_duration` + `u_lod_fade` | 父子块同时渲染，shader 按屏幕哈希互补 discard，掩盖 popping |

**godot_voxel 的方块 LOD 没有 graft/缝合，只有 skirt。** 平滑地形才用 transvoxel。

godot 的 `append_side_skirts` 逐行语义（便于和我们的实现对照）：

- 遍历块侧面**外层（outer / padding）**的 2D 格 `v`；`v == AIR` 跳过；
- `v` 的 4 个**切向**邻格全实心 -> 跳过（没有暴露的边）；
- 沿法线向块内一格 `nv4`（inner）必须实心，否则跳过；
- 用 `nv4` 的模型/材质在边界平面补一条侧面 quad；AO 不处理。

## 3. 分区：`LodQuadTree` + 2:1 平衡

`game-core/src/voxel/terrain/blocks.rs::lod_blocks_for_observer` 委托
`crates/voxel/src/lod_quadtree.rs`（自下而上 `base_merge` + 2:1 平衡），
输出仍是 `Vec<TerrainBlock>`，签名与契约不变。**不是跨帧渐进树**，每次全量重算。

不变量：

1. 区域 `x,z ∈ [-r,r]`、`y ∈ [0,layers)` 的每个基础子块恰好被一个
   `TerrainBlock` 覆盖一次，原点对齐 `2^lod`。
2. XZ 4 邻叶子的 lod 差 <= 1（`balance_2_to_1`）；因此一个面的外侧只会是
   `Same` / `Coarser` / `Finer` / `Missing` 之一。
3. 一切确定性：BTreeMap/BTreeSet + 排序输出；相同 `(observer, config)` ->
   相同分区与相同 stable id 分配顺序。

## 4. LOD 块生成（阶段 A）

### 4.1 API

```rust
// game-core/src/voxel/terrain/generation.rs
WorldGenerator::generate_block_at_lod(interner, origin, lod) -> VoxTree<u8>
```

- 块的 `MaxDepth = CHUNK_DEPTH + lod`，mesher 采样到 32³ 粗格；每格边长
  `span = 2^lod` 个基础体素。
- 高度采样：每列取该列 `span × span` 个细列里**中心那一列**的
  `column_surface`（等价于密度场按 LOD 采样）。刻意不用 max（粗 LOD 变胖、
  出唇边），也不用平均（台阶被抹平、地表塌到地下）。
- 材质分层：粗格材料按「格顶面体素在地表下的深度」决定；含地表的粗格永远
  取 `palette.surface`，所以粗格顶面不会露出 DIRT / STONE。
- 内部用 32³ uniform 叶 + 逐层哈希折叠，全空气 / uniform 子树会塌成单叶，
  不会展开成 `32·2^lod` 个细体素。
- 纯函数：只依赖 `(origin, lod)`，不读其它块（L3 确定性）。

`game-core/src/voxel/terrain/generated.rs::GeneratedBlockCache` 按
`(origin, lod)` 缓存 **LOD>0** 的生成树；`invalidate_covered` / `clear` 显式
`dec_ref_recursive`。LOD0 仍走
`game_engine::presentation::voxel::WrappedBlockCache`（逐位不变）。

### 4.2 已删除的旧生成路径

**旧方案（已弃用并删除）**：LOD>0 时先用 `wrap_block` 把 `2^lod` 个 LOD0
子块折叠成粗树，节点代表值由 `calc_average` / `VoxelTrait::average` 对 8 个
子节点投票。**「棕色顶面」的历史根因**：折叠后的粗格节点值来自 8 个 LOD0
子节点的投票；当 8 个子节点都在地下时，代表值被投成 DIRT / STONE，而这个
值又被当作粗格的「地表材质」画在顶面上——于是远看整片地表变成地下材质。
同时粗格高度也可能整体下沉。这正是 A 阶段改为按自身 LOD 生成的原因。

- 地形生成不再调用这条路径（a96c5a8）。
- `wrap_block` 仍保留：用于 LOD0 子块包装、authoring/测试路径，以及
  `extract_block_tree` 的兼容入口；它不再是 LOD>0 terrain 的生成方式。
- `calc_average` 仍保留：服务于 `VoxTree` 自身的 average 语义（其它降采样
  路径），但不参与 LOD 地形生成。

## 5. 邻块报告与自闭合（阶段 B）

### 5.1 `FaceNeighbor`（terrain 层）

`game-core/src/voxel/terrain/blocks.rs`：

```rust
pub enum FaceNeighbor {
    Missing,
    Same(ChunkKey),
    Coarser { origin: ChunkKey, offset: [u8; 2] }, // 邻居 lod 大 1
    Finer([ChunkKey; 4]),                          // 邻居 lod 小 1，切向 2x2
}
```

- `neighbour_lod_report` 给出完整报告，只用于「这面是否跨 LOD」（裙边掩码 /
  组重建签名）。
- `face_neighbors` 在报告之上把 `Coarser` / `Finer` **一律映射成 Missing**；
  mesh 层只看到同 LOD 邻居。
- `lod_mismatch_mask` = 只有 `Coarser` / `Finer` 面置位。
- `missing_face_mask` = `Missing` 面置位。
- 实际载荷 mask = `lod_mismatch_mask | missing_face_mask`。

### 5.2 为什么删除跨 LOD 面剔除

旧实现把粗邻居按 2x2 保守聚合、细邻居按 2x2 任一实心采样，两侧各自剔除。
问题是同一块面在两侧的采样粒度不同，容易出现「细侧认为被粗格遮挡 -> 剔除，
粗侧认为被细格遮挡 -> 也剔除」，形成穿透洞。现在的做法：

- 跨 LOD 面在 `face_neighbors` 里就是 Missing -> 两侧都不剔除 -> 各画各的
  边界墙面，几何上自闭合；
- 跨 LOD halo 采样（`build_tree_occupancy_lod` 的 Coarser/Finer 分支、
  `extract_raw_halo_with_faces` 的 2x2 聚合）已整体删除；
- mesh 层 `crates/voxel/src/mesh/occupancy.rs::FaceNeighbor` 只剩
  `Missing` / `Same` 两个变体，`occupancy.rs` 中不再有 coarser/finer 分支。

### 5.3 halo 提取

`extract_block_raw`（`terrain/mod.rs`）：

1. 先预热同 LOD 的 26 邻（6 面 + 12 棱 + 8 角）；LOD>0 从
   `GeneratedBlockCache` 取，LOD0 从 `WrappedBlockCache` 取；缺失 = 空气。
2. 面邻居只按同 LOD 报告解析成 mesh 层 `FaceNeighbor::Missing/Same`。
3. `extract_raw_halo_with_faces` 产出 34³：内部 32³ + 6 面 external + 12 棱 /
   8 角逐格采样。
4. 棱 / 角邻块必须与面报告一致（`edge_corner_supported`）：只要它依赖的任一
   面邻居是 Missing，就不再从 margin 块读实心；否则外墙面 AO 会在块边界被
   误遮挡（表现为一格宽的暗线）。

### 5.4 自闭合与 AO

- 每块 32³ 的内部面 + 自己的边界墙面构成闭合壳；同 LOD 邻居之间的面仍按
  external 掩码正常剔除，避免块内重复几何。
- AO 只用于真实绘制的面；裙边不参与 AO（见 §6）。
- 缺失邻块按空气处理，所以 mesh 区域外缘也会封边（不会看到块内部）。

## 6. 接缝裙边（阶段 C）

### 6.1 mask 语义

`skirt_mask` 6 bit，bit `face` 对应 `ExternalPlane` 顺序
`[+x, -x, +y, -y, +z, -z]`：

- cross-LOD 错位面（`lod_mismatch_mask`）；
- 面外没有同 LOD 邻居的面（`missing_face_mask`，缺失按空气处理）。

wire 格式不变：34³ raw 字节 + 1 字节 mask；分组载荷每块带自己的 mask。
`skirt_mask == 0` 时 mesher 输出与不开裙边逐位相同。

### 6.2 生成规则（`crates/voxel/src/mesh/greedy.rs::append_skirts`）

对 mask 置位的每个面，遍历该边界平面的 32×32 格：

1. **覆盖位图**：主 mesher 在该 `(plane, dir, slice)` 已经画过的格（大矩形会
   展开成多格）直接跳过，防止共面重复 / z-fighting；
2. **边界格实心**（块内边界层 `n-1` / `0`）；
3. **内侧格实心**（再向块内一格 `n-2` / `1`）——避免给一格薄片补墙；
4. **切向暴露**：边界格的 4 个切向邻格至少一个空气（±X 面看 y/z，±Y 面看
   z/x，±Z 面看 y/x）；
5. 通过后补一条 1x1、位置在边界平面的 rect，材质取**内侧格**，AO 固定
   `[3, 3, 3, 3]`。

与 godot 的差异（有意保留，需知悉）：godot 用 outer（padding）格 + inner
（块边界）格，材质/模型取 inner；我们的实现不看面外邻居，把判据整体向块内
偏了一格（块边界 + 再向里一格），材质取再向里一格。副作用：

- 1 格厚的墙（边界实心、再向里是空气）补不出裙边；
- 材质可能取到地表下一层（例如 DIRT 而不是 GRASS）。

将来真要严格对齐 godot 的正确做法：

- 判据改为 **outer halo 格（面外）+ 内边界格 `n-1` + outer 的切向暴露**；
- 材质 / 模型取 `n-1`（内边界格），不是 `n-2`；
- 这需要重新引入面外采样，等价于部分撤销 B（e69349b）的「只看同 LOD 邻居」，
  必须单独评估；**不要**只为了字面对齐就恢复跨 LOD 面外剔除。

### 6.3 现状与限制（重要）

当前 `extract_raw_halo_with_faces` 对所有 masked 面（cross-LOD / Missing）
都把面外 halo 层填成**空气**：`build_tree_occupancy_lod` 对 Missing 不填
external，26 邻只填棱 / 角。因此主 mesher 在这些面已经画出全部边界格，
覆盖位图全部命中，`append_skirts` 在真实管线里通常**一条几何都不产出**；
把 mask 扩到 `missing_face_mask` 只会让 mask 多置位，不会让几何变多。

> 一句话：**块自闭合后，这些边界面已由主 mesher 覆盖；裙边只在面外残留
> 实心 halo（跨 LOD 采样回归）、或人造 halo 的测试场景下才会产出几何。
> 当前 extract 路径下它是 no-op，保留作为正确性安全网 + 切向轴修复。**

**接缝的可见表现是台阶墙，不是洞。** 两个不同 LOD 的闭合壳在交界处各画各的
边界墙，几何上没有穿透；看到的是高度/材质不连续的台阶。压台阶应该用：

- LOD fade：`VoxelMeshNode.lod_fade_duration`（shader 的 `u_lod_fade` 互补
  discard），默认 0 = 关闭；
- 或各向异性 LOD（只降水平分辨率、不降垂直分辨率）；

而不是靠持续加裙边几何。裙边只在面外实心的残留场景下作为安全网生效。
如果将来要重新引入跨 LOD 面外采样（与 B 冲突），必须同时补上 §6.2 的
outer 判据，否则又会回到「两侧都剔除」的穿透洞。

### 6.4 渲染开关

开关在渲染侧：`voxel_mesh.gd` 的 `VoxelMeshNode.lod_skirts_enabled`（静态，
默认 on）+ `bevy_client.gd` 的 `voxel_lod_skirts` 导出；关闭时给 mesher 传
`skirt_mask = 0`（对应 godot_voxel 的 per-model `lod_skirts_enabled`）。
Rust 侧始终把 mask 算进载荷，不做二次开关。

同一个文件里紧挨着的是 LOD 渐隐开关 `lod_fade_duration`（默认 0 = 关闭）；
它才是压 LOD 台阶的手段，见 §6.3。

## 7. 分组与 wire

`GroupKey { lod, rx, ry, rz }`，`r* = origin.*.div_euclid(REGION_BLOCKS << lod)`，
`REGION_BLOCKS = 2`。组内所有块同 LOD，因此单 `lod` 字段足够。
`TerrainGroups` 按组做 diff；成员集或邻块 LOD 变化（影响 halo / mask）的组
才重建。stable id 逐组分配。

- `VoxChunkGroupRaw.chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)>`：每块 34³ raw +
  mask；
- `RawVoxelPayload` / `packed` 的逐块记录头：`x,y,z, byte_len, skirt_mask,
  chunk_count`；
- Godot `voxel_group.gd` 把每块 mask 透传给子 `VoxelMeshNode`；
- 34³ 字节布局与 `lod` 语义不变，旧 wire 解码器可继续工作。

## 8. 验收

- `cargo fmt --all --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`（注：当前仓库 HEAD
  存在一批与 LOD 无关的预存 lint；见提交报告）
- 不变量：
  - LOD0 输出逐位不变；
  - `skirt_mask == 0` 输出逐位不变；
  - 34³ raw + 1 字节 mask 的 wire 格式、FFI、GDScript 不变；
  - `cargo bench -p game-core --bench voxel_terrain`（若可运行）。

单测重点：

- `crates/voxel/src/mesh/tests.rs`：主 mesher / AO 黄金；
- `game-engine/.../raw.rs`：`raw_halo_equivalence_matches_direct_mesher`、
  `raw_halo_external_seam_matches_direct_mesher`、`skirts_*`（mask 面、
  覆盖去重、切向两轴、材质 / AO）；
- `game-core/.../terrain`：分区 2:1、`face_neighbors`、mask 端到端、
  分组 diff / 重建签名。

## 9. 风险

- **接缝是台阶墙、不是洞**：自闭合之后没有穿透洞；视觉不连续来自 LOD 台阶，
  要靠 fade / 各向异性 LOD，不是裙子。
- **当前裙边在自闭合路径下是 no-op**（§6.3）：mask 面面外是空气，主 mesher
  已经封边。真正的面外补缝需要跨 LOD 面外信息，和 B 的原则冲突。
- **判据与 godot 差一格**（§6.2）：1 格厚墙漏补、材质可能取深一层；这项目前
  不影响画面（no-op），但将来真做裙边时必须先对齐。
- **2:1 balance 改变分区**：块数 / stable id 会随观察者变化；回放 / 基线
  测试需按同一 `(observer, config)` 重算。
- 无 Godot 二进制时只能静态验证 GDScript / FFI 契约；Rust 网格测试兜底。
