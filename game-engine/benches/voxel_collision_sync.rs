//! 体素碰撞同步基准：8 个 root × 512 个 chunk 的增量同步。
//!
//! 运行：
//!   cargo bench -p game-engine --features voxel,physics --bench voxel_collision_sync
//!
//! 8 个 root 各自 512 个 chunk（8³ 的实心网格，所有 6 邻都存在）。每个 tick 都让
//! 8 个 root 同时命中 `Or<(Added<VoxVolume>, Changed<VoxVolume>, Added<VoxelCollider>)>`，
//! 于是既能测到「每个 root 重扫全部 chunk 实体」的 O(roots × chunks) 扫描，
//! 也能测到逐块 content_hash。
//!
//! 两种 tick：
//! - `touch_only`：VoxVolume 被标记为改动但内容没变（整体替换成相同内容 / 无操作编辑）。
//!   全部哈希相等 -> changed_keys 为空，只剩「扫描 + 哈希」。
//! - `edit_one_voxel`：每个 root 改一个体素。除扫描 + 哈希外，还要重建该 chunk 及其
//!   6 邻（32³ 逐个 `get` + 构造 parry Voxels），代表真实编辑 tick。
//!
//! 两种 chunk 密度（同一台机器上对照，避免只挑有利工作量）：
//! - `dense64`：每块 4×4×4 = 64 个体素（接近实心地形的哈希成本）；
//! - `sparse8`：每块 2×2×2 = 8 个体素（薄壁 / 稀疏编辑，扫描占比更高）。
//!
//! 基准直接跑 `FixedUpdate` 调度（不跑 Main），计时里不含渲染 / 物理开销。

use bevy::math::IVec3;
use bevy::prelude::*;
use criterion::{criterion_group, criterion_main, Criterion};
use game_engine::math::FixedPoint;
use game_engine::voxel::{
    set_voxel, ChunkKey, MaxDepth, VoxOpsWrite, VoxTree, VoxVolume, VoxelCollider,
    VoxelCollisionChunk, VoxelCollisionPlugin, VoxelDirtySet, VoxelInterner, VoxelPlugin,
    CHUNK_DEPTH,
};

/// root 数量。
const ROOTS: usize = 8;
/// 每个 root 的 chunk 数量（8³ 的实心网格）。
const CHUNKS_PER_ROOT: usize = 512;

/// chunk 的体素密度。
#[derive(Clone, Copy)]
enum Density {
    /// 4×4×4 = 64 个体素。
    Dense64,
    /// 2×2×2 = 8 个体素。
    Sparse8,
}

impl Density {
    fn label(self) -> &'static str {
        match self {
            Density::Dense64 => "dense64",
            Density::Sparse8 => "sparse8",
        }
    }

    /// 立方体边长。
    fn side(self) -> i32 {
        match self {
            Density::Dense64 => 4,
            Density::Sparse8 => 2,
        }
    }
}

/// 第 i 个 chunk 的 body 局部坐标（0..8 ³）。
fn chunk_key(i: usize) -> ChunkKey {
    ChunkKey::new((i % 8) as i32, ((i / 8) % 8) as i32, (i / 64) as i32)
}

/// 第 i 个 chunk 的确定性内容：一个边长 `side` 的实心立方体，位置随 i 变化，
/// 这样 interner 里有多种子树形状，哈希不会全部走同一条缓存路径。
fn chunk_tree(interner: &mut VoxelInterner, i: usize, side: i32) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    let ox = (i % 5) as i32 * 6;
    let oy = ((i / 5) % 5) as i32 * 6;
    let oz = ((i / 25) % 5) as i32 * 6;
    for x in 0..side {
        for y in 0..side {
            for z in 0..side {
                let _ = tree.set(interner.inner_mut(), IVec3::new(ox + x, oy + y, oz + z), 1);
            }
        }
    }
    tree
}

struct Harness {
    app: App,
    roots: Vec<Entity>,
    /// 下一次编辑要写的体素值（0/1 交替，保证每次 tick 内容都真的变了）。
    next_value: u8,
}

/// 当前全部 chunk 碰撞实体数量。
fn chunk_count(world: &mut World) -> usize {
    world.query::<&VoxelCollisionChunk>().iter(world).count()
}

fn build_harness(density: Density) -> Harness {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(VoxelInterner::new(256 * 1024 * 1024));
    app.add_plugins(VoxelPlugin::default());
    // 只跑碰撞派生：不装 avian 的 PhysicsPlugins，避免物理系统混进计时。
    app.add_plugins(VoxelCollisionPlugin {
        ensure_physics: false,
    });

    let mut roots = Vec::with_capacity(ROOTS);
    {
        let world = app.world_mut();
        let mut interner = world.remove_resource::<VoxelInterner>().expect("interner");
        for _ in 0..ROOTS {
            let mut volume = VoxVolume::new(FixedPoint::from_num(1));
            for i in 0..CHUNKS_PER_ROOT {
                let tree = chunk_tree(&mut interner, i, density.side());
                volume.insert_chunk(chunk_key(i), tree);
            }
            roots.push(world.spawn((volume, VoxelCollider)).id());
        }
        world.insert_resource(interner);
    }

    // 首次同步：把 4096 个 chunk 实体全部派生出来（不计入基准）。
    app.world_mut().run_schedule(FixedUpdate);
    let spawned = chunk_count(app.world_mut());
    assert_eq!(
        spawned,
        ROOTS * CHUNKS_PER_ROOT,
        "首次同步必须派生 roots × chunks 个碰撞实体"
    );

    Harness {
        app,
        roots,
        next_value: 0,
    }
}

/// 标记 VoxVolume 为 Changed 但不改内容：模拟「整体替换成相同内容 / 无操作编辑」。
fn touch_volume(world: &mut World, root: Entity) {
    let mut entity = world.entity_mut(root);
    let mut volume = entity
        .get_mut::<VoxVolume>()
        .expect("root 必须带 VoxVolume");
    // 只标脏：内容原样保留，让 Changed 过滤命中但哈希全部相等。
    volume.set_changed();
}

/// 跑一个同步 tick。
fn sync_once(harness: &mut Harness, edit: bool) {
    let roots = harness.roots.clone();
    if edit {
        let value = harness.next_value;
        harness.next_value = 1 - value;
        for root in roots {
            let mut dirty = VoxelDirtySet::new();
            harness
                .app
                .world_mut()
                .resource_scope::<VoxelInterner, _>(|world, mut interner| {
                    let mut entity = world.entity_mut(root);
                    let mut volume = entity.get_mut::<VoxVolume>().expect("root 带 VoxVolume");
                    // 改 (1,1,1)：该 chunk 及其 6 邻会重建。
                    let _ = set_voxel(&mut volume, &mut interner, &mut dirty, [1, 1, 1], value);
                });
        }
    } else {
        for root in roots {
            touch_volume(harness.app.world_mut(), root);
        }
    }
    harness.app.world_mut().run_schedule(FixedUpdate);
}

fn bench_sync_collision(c: &mut Criterion) {
    for density in [Density::Dense64, Density::Sparse8] {
        let mut harness = build_harness(density);
        // 预热一次编辑 tick，让改动的 chunk 走完一次重建路径。
        sync_once(&mut harness, true);
        println!(
            "sync_collision/{}: roots={} chunks_per_root={} chunk_entities={}",
            density.label(),
            ROOTS,
            CHUNKS_PER_ROOT,
            ROOTS * CHUNKS_PER_ROOT
        );

        let label = density.label();
        c.bench_function(&format!("sync_collision/{label}/touch_only"), |b| {
            b.iter(|| sync_once(&mut harness, false))
        });
        c.bench_function(
            &format!("sync_collision/{label}/edit_one_voxel_per_root"),
            |b| b.iter(|| sync_once(&mut harness, true)),
        );

        // 结束状态自检：实体数不因反复同步而漂移。
        assert_eq!(
            chunk_count(harness.app.world_mut()),
            ROOTS * CHUNKS_PER_ROOT
        );
    }
}

criterion_group!(benches, bench_sync_collision);
criterion_main!(benches);
