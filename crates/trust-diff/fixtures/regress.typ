// 差分回归:错值/崩溃级修复(A 批)与错误路径文案(B 批)的锚点。
// 每个绑定对照一项修复,调用用例见 tests/diff.rs 的 cases("regress")。

// A1:状态函数带解构参数(stub 签名必须绑定合成名才能转发)。
#let base = 10
#let pat((a, b), k: 2) = a + b * base + k

// A3:多段赋值末字段缺失即创建;中间键缺失报错(错误用例)。
#let deep() = {
  let d = (a: (b: 1))
  d.a.c = 5
  d
}
#let mid-missing() = {
  let d = (a: 1)
  d.x.y = 2
  d
}
#let compound-missing() = {
  let d = (a: 1)
  d.zz += 1
  d
}

// A4:while 有界循环的值;静态常真与迭代护栏两个报错路径。
#let count-to(n) = {
  let i = 0
  while i < n { i += 1 }
  i
}
#let always-true() = { while true {} }
#let spin() = { let n = 0; while n < 5 { n = 0 } }

// A5:代码块值 = 全部语句值的 join(字符串拼接/none 透明;不可 join 报错)。
#let block-join() = { "a"; "b"; "c" }
#let block-none() = { none; 1 }
#let block-tail-none() = { 1; none }
#let block-join-fail() = { 1; 2 }

// A6:命名参数默认值单次求值(缺参取默认)。
#let pow2 = 3
#let def-ok(x: 2 * pow2) = x

// B1:被调值不可调用(cast 文案;实参不被求值)。
#let not-fn = 5

// B7:dict 键方法调用/无此方法的两段式报错(错误用例)。
#let mk() = (a: 1)

// B10:自递归走到深度护栏(typst 闭包创建期捕获,顶层互引不可见,
// 故用自递归验证经 stub 的调用深度护栏)。
#let down(n) = if n == 0 { 0 } else { down(n - 1) }

// B6:元素函数字段的样式链取值(context 内)。
#let size-of() = context text.size

// Equation:math 按源码片段解释求值(仅此一类走 eval;符号与外层
// 变量捕获两种形态)。
#let eq-simple() = $1 + 1$
#let eq-sym() = $arrow$
#let eq-captured(x) = $x + 1$

// C2 值位置闭包(体 > 32 节点走原生):多捕获 + 算术混合。
#let base-scale = 1pt
#let big-closure(a, b) = {
  let f = (x) => {
    let acc = a * x + b
    let res = acc * 2 + b - a
    let more = res + acc - b + a
    let s1 = more * 3 + acc - res + b * b - a
    let s2 = s1 + more - acc + res * 2 - b + a
    s2 + s1 + more + acc + res + a + b
  }
  f(2) + f(3)
}

// C2 自递归闭包:命名闭包体引用自身(原生路径,SelfFuncCell)。
#let self-rec(n) = {
  let countdown(k) = {
    if k <= 0 { 0 } else if k == 1 { 1 } else { k + countdown(k - 1) }
  }
  countdown(n)
}
