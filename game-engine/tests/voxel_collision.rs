//! 体素碰撞派生集成测试：VoxVolume + VoxelCollider 派生 per-chunk avian Collider；
//! 移除 marker 后同帧撤销全部 chunk 碰撞实体。
//!
//! 运行：
//!   cargo test -p game-engine --features voxel,physics --test voxel_collision
#![cfg(all(feature = "voxel", feature = "physics"))]

use bevy::prelude::*;
use bevy::time::Fixed;
use game_engine::math::FixedPoint;
use game_engine::voxel::{
    set_voxel, Collider, RigidBody, VoxVolume, VoxelCollider, VoxelCollisionChunk,
    VoxelCollisionPlugin, VoxelDirtySet, VoxelInterner, VoxelPlugin,
};
use voxel::store::{ChunkKey, MaxDepth, VoxOpsBulkWrite, VoxTree, CHUNK_DEPTH};

/// 无头测试里虚拟时间增量可能不足一个 fixed step，手动累积一个步长的 overstep，
/// 保证本次 update() 至少执行一次 FixedUpdate（accumulate_overstep 是 bevy_time
/// 为测试提供的公开 API）。
fn run_fixed_update(app: &mut App) {
    let step = app.world().resource::<Time<Fixed>>().timestep();
    app.world_mut()
        .resource_mut::<Time<Fixed>>()
        .accumulate_overstep(step);
    app.update();
}

fn full_chunk(interner: &mut VoxelInterner) -> VoxTree<u8> {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    tree.fill(interner.inner_mut(), 1);
    tree
}

/// 某个 root body 当前的全部 chunk 碰撞实体（Entity 升序，便于断言）。
fn chunk_entities(world: &mut World, body: Entity) -> Vec<Entity> {
    let mut query = world.query::<(Entity, &VoxelCollisionChunk)>();
    let mut out: Vec<Entity> = query
        .iter(world)
        .filter(|(_, chunk)| chunk.body == body)
        .map(|(entity, _)| entity)
        .collect();
    out.sort();
    out
}

fn test_app(interner: VoxelInterner) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(interner);
    app.add_plugins(VoxelPlugin::default());
    app.add_plugins(VoxelCollisionPlugin::default());
    // Avian 在 Plugin::finish 里注册 SpatialQueryDiagnostics / ColliderTreeDiagnostics；
    // 只用 app.update() 的测试必须显式 finish，否则 PhysicsSchedule 的系统参数校验失败。
    app.finish();
    app
}

/// 无 RigidBody 的 root：chunk 走顶层静态路径（无 ChildOf），root 自身不挂 Collider；
/// 新增 chunk 时增量派生，移除 marker 时同帧撤销。
#[test]
fn per_chunk_colliders_are_derived_and_revoked_for_static_root() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let tree = full_chunk(&mut interner);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    volume.insert_chunk(ChunkKey::new(0, 0, 0), tree);

    let mut app = test_app(interner);
    let root = app.world_mut().spawn((volume, VoxelCollider)).id();

    run_fixed_update(&mut app);
    let chunks = chunk_entities(app.world_mut(), root);
    assert_eq!(
        chunks.len(),
        1,
        "一个实心 chunk 必须派生一个 chunk 碰撞实体"
    );
    assert!(
        app.world().entity(root).get::<Collider>().is_none(),
        "root 自身不应直接挂 Collider"
    );
    for &chunk_entity in chunks.iter() {
        assert!(
            app.world().entity(chunk_entity).get::<Collider>().is_some(),
            "chunk 实体必须挂 Collider"
        );
        assert!(
            app.world().entity(chunk_entity).get::<ChildOf>().is_none(),
            "无 RigidBody 的 root 的 chunk 必须是顶层实体（无反推的 ColliderOf 路径）"
        );
        let chunk = app
            .world()
            .entity(chunk_entity)
            .get::<VoxelCollisionChunk>()
            .copied()
            .unwrap();
        assert_eq!(chunk.body, root);
        assert_eq!(chunk.key, ChunkKey::new(0, 0, 0));
    }

    // 新增一个 chunk：Changed<VoxVolume> + per-chunk hash diff 应增量派生第二个实体。
    let new_tree = {
        let mut interner = app.world_mut().resource_mut::<VoxelInterner>();
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        tree.fill(interner.inner_mut(), 1);
        tree
    };
    {
        let mut root_entity = app.world_mut().entity_mut(root);
        let mut volume = root_entity
            .get_mut::<VoxVolume>()
            .expect("root 应带 VoxVolume");
        volume.insert_chunk(ChunkKey::new(1, 0, 0), new_tree);
    }
    run_fixed_update(&mut app);
    let chunks = chunk_entities(app.world_mut(), root);
    assert_eq!(
        chunks.len(),
        2,
        "新增 chunk 后必须派生第二个 chunk 碰撞实体"
    );

    // 移除 marker：cleanup 同帧撤销全部 chunk 实体。
    app.world_mut().entity_mut(root).remove::<VoxelCollider>();
    run_fixed_update(&mut app);
    assert!(
        chunk_entities(app.world_mut(), root).is_empty(),
        "移除 VoxelCollider 后必须撤销全部派生 chunk 实体"
    );
}

/// T4 acceptance: a single voxel edit through `set_voxel` changes the volume,
/// and the VoxelSet::Collision hash diff must rebuild that chunk collider.
#[test]
fn single_voxel_edit_rebuilds_the_affected_collision_chunk() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let tree = full_chunk(&mut interner);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    volume.insert_chunk(ChunkKey::new(0, 0, 0), tree);

    let mut app = test_app(interner);
    let root = app.world_mut().spawn((volume, VoxelCollider)).id();
    run_fixed_update(&mut app);

    let chunk = chunk_entities(app.world_mut(), root)[0];
    let before_hash = app
        .world()
        .entity(chunk)
        .get::<VoxelCollisionChunk>()
        .expect("collision chunk")
        .content_hash;

    let mut dirty = VoxelDirtySet::new();
    app.world_mut()
        .resource_scope::<VoxelInterner, _>(|world, mut interner| {
            let mut root_entity = world.entity_mut(root);
            let mut volume = root_entity
                .get_mut::<VoxVolume>()
                .expect("root must keep its VoxVolume");
            assert_eq!(
                set_voxel(&mut volume, &mut interner, &mut dirty, [5, 6, 7], 0),
                Ok(true)
            );
        });

    run_fixed_update(&mut app);

    let after = app
        .world()
        .entity(chunk)
        .get::<VoxelCollisionChunk>()
        .expect("chunk must still exist");
    assert_ne!(
        before_hash, after.content_hash,
        "the edited chunk's structural hash must change"
    );
    assert!(
        app.world().entity(chunk).get::<Collider>().is_some(),
        "the rebuilt chunk must keep its avian Collider"
    );
}

/// 带 RigidBody 的 root：chunk 必须是它的子实体，交给 avian 的
/// ColliderHierarchyPlugin 自动补 ColliderOf / ColliderTransform。
#[test]
fn per_chunk_colliders_are_children_of_rigid_body_root() {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let tree = full_chunk(&mut interner);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    volume.insert_chunk(ChunkKey::new(0, 0, 0), tree);

    let mut app = test_app(interner);
    let root = app
        .world_mut()
        .spawn((volume, VoxelCollider, RigidBody::Static))
        .id();

    run_fixed_update(&mut app);
    let chunks = chunk_entities(app.world_mut(), root);
    assert_eq!(chunks.len(), 1);
    for &chunk_entity in chunks.iter() {
        assert_eq!(
            app.world()
                .entity(chunk_entity)
                .get::<ChildOf>()
                .map(ChildOf::parent),
            Some(root),
            "带 RigidBody 的 root 的 chunk 必须挂 ChildOf(root)"
        );
    }
}
