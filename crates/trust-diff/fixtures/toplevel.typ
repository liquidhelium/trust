// 顶层引擎值:模块加载期即求值的函数调用与标准库引用。
#let make-table(n) = {
  let out = ()
  for i in range(n) {
    out.push(i * i)
  }
  out
}
#let squares5 = make-table(5)
#let golden-name = upper("trust")
#let mean(a, b) = (a + b) / 2
#let mean-of = mean(squares5.at(1), squares5.at(2))
