// 真实回归用例:官方套件未覆盖、翻译器曾经出错的形态。
// 全部为干净用例(golden 必过),值级一致用 #test 断言。

--- reg-destruct-key-not-capture eval ---
// cetz draw.scope 形态:解构赋值的字典键不是名字引用——曾把 `bounds`
// 键收进闭包捕获集,调用期报 unknown variable: bounds。
#let scope(start) = (ctx) => {
  let ctx = start
  let drawables = ()
  let bounds = none
  (ctx: ctx, drawables, bounds: _) = (ctx: (start + 1), drawables: (1, 2), bounds: none)
  drawables.at(0) + ctx
}
#test(scope(41)(0), 43)

--- reg-destruct-array-pattern-binds eval ---
// `(a, b) = ...` 的模式名是绑定不是引用(circular 的 bounds 案同根)。
#let f() = {
  let a = 0
  let b = 0
  (a, b) = (10, 20)
  a + b
}
#test(f(), 30)

--- reg-bare-string-import-binds-stem eval ---
// cetz-plot setup-axes 形态:体内裸导入 "leaf.typ" 绑定文件名主干
// `leaf`——曾用 rsplit('.') 绑成 `typ`,引用走全局回退报 unknown
// variable: axes;依赖图缺体内边时槽未初始化(evaluation-order error)。
#let setup() = {
  import "leaf.typ"
  leaf.pub-fn(1)
}
#test(setup(), 2)

--- reg-bare-package-import-name eval ---
// 裸模块值导入绑链尾名(cetz `import cetz.draw` 形态的静态推导分支)。
#import "outer.typ": cetzlib
#import cetzlib.draw
#test(draw.line(), 1)

--- reg-wildcard-body-no-capture eval ---
// cetz legend 形态:闭包体内 `import m: *` 的名字不走创建期捕获——
// 曾进 pending 捕获,调用期 cap(None) 硬报 unknown variable。
#import "leaf.typ" as leaf
#let draw-legend(items) = leaf.wrap((ctx) => {
  import leaf: *
  set-mark(items) + pub-fn(items.len())
})
#test(draw-legend((1, 2, 3)), 7)

--- reg-captured-frame-shadow eval ---
// fletcher/cetz data-viewport 形态:外层闭包捕获 `body`,内层同名参数
// 赋值——捕获只读判定曾跨层帧误伤,发射成 bail_captured。
#let outer(body) = (x) => {
  let inner(body, k) = {
    if k > 0 { body = body + 1 }
    body * 10
  }
  inner(body, x)
}
#test(outer(4)(1), 50)

--- reg-inner-let-shadows-capture eval ---
// 同名内层 let 遮蔽外层捕获(捕获只读判定的另一半)。
#let outer2(v) = (x) => {
  let r = {
    let v = v + 1
    v = v * 2
    v
  }
  r + x
}
#test(outer2(10)(1), 23)
