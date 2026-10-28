// 回归:in-body 嵌套 import item(`import <模块值>: a.b as c`)必须逐组件
// 走链(先 a 再 b),不能只取路径末段做单次字段访问。经由 outer →
// cetzlib → draw 的真实模块链覆盖重命名与不重命名两种形态。

--- reg-in-body-nested-import-item eval ---
#import "outer.typ": cetzlib
#let v = {
  import cetzlib: draw.line as ln
  ln()
}
#test(v, 1)

--- reg-in-body-nested-import-item-plain eval ---
#import "outer.typ": cetzlib
#let u = {
  import cetzlib: draw.line
  line()
}
#test(u, 1)
