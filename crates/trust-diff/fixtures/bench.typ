// Bench workload: pure loops (no closure-cache dividend), string accumulation, closure factories.
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
#let inc(x) = x + 1
#let call-loop(n) = {
  let x = 0
  for i in range(n) {
    x = inc(x)
  }
  x
}
#let map-heavy(n) = range(n).map(i => i * 2).filter(i => calc.even(i)).len()
