// 基准负载:纯循环(无闭包缓存红利)、字符串累积、闭包工厂。
#let fib-mod(n) = {
  let a = 0
  let b = 1
  for _ in range(n) {
    let t = calc.rem(a + b, 1000000007)
    a = b
    b = t
  }
  a
}
#let sum-nums(n) = {
  let total = 0
  for i in range(n) {
    total = total + i
  }
  total
}
#let repeat-join(n) = {
  let out = ""
  for _ in range(n) {
    out = out + "x"
  }
  out.len()
}
#let make-fib(k) = {
  let fib(n) = if n <= 1 { n } else { fib(n - 1) + fib(n - 2) }
  fib
}
