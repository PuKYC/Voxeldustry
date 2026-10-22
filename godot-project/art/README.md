# art/ —— 美术源图（不参与运行时，也不进导出包）

本目录带 `.gdignore`，Godot 不会扫描 / 导入这里，导出包也不会包含它。

bake 工具（`tools/bake_voxel_materials.gd`）会优先读取：

```
voxel_materials/<material_key>_albedo.png
voxel_materials/<material_key>_normal.png
voxel_materials/<material_key>_orm.png
voxel_materials/<material_key>_emissive.png
```

- albedo / emissive：sRGB 编码 PNG（`albedo_texture_srgb=true` 时 bake 会转线性）；
- normal：线性数据，OpenGL +Y 约定（DirectX 绿通道朝下时勾 `normal_flip_green`）；
- orm：R=AO、G=roughness、B=metallic，线性数据；
- 尺寸随意，bake 会 resize 到 `library.tile_size`；建议 64/128/256 且 2 的幂。

改完源图后重跑 bake，`tiles/*.res` 会更新；运行时只读 `.res`，永远不直接读 PNG。
