class_name EntityInterpolator
extends RefCounted
## 通用驱动：只遍历活动实体。活动与否由 dispatcher 依据 PayloadCodec.is_active
## 判定；插值本身只调 dispatcher.drive / drive_entity —— 不认识任何具体载荷。

const STALE_TICKS := 3

var _registry: EntityRegistry
var _dispatcher: PresentationDispatcher


func _init(reg: EntityRegistry, disp: PresentationDispatcher) -> void:
	_registry = reg
	_dispatcher = disp


func tick(alpha: float, current_tick: int) -> void:
	var active := _registry.active()
	var i := active.size() - 1
	while i >= 0:
		var e: EntityData = active[i]
		if current_tick - e.last_tick > STALE_TICKS:
			_dispatcher.drive_entity(e, 1.0)  # alpha=1 落位到最终值
			_registry.set_active(e, false)
		i -= 1
	# 剩下的活动实体统一按 alpha 采样。
	_dispatcher.drive(_registry.active(), alpha)
