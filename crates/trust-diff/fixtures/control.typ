// 控制流与递归:if 链、while、for join、局部绑定与遮蔽。
#let fact(n) = if n <= 1 { 1 } else { n * fact(n - 1) }
#let fib(n) = {
  let a = 0
  let b = 1
  for _ in range(n) {
    let next = a + b
    a = b
    b = next
  }
  a
}
#let sum-to(n) = {
  let total = 0
  let i = 1
  while i <= n {
    total = total + i
    i = i + 1
  }
  total
}
#let greet(name, formal: false) = if formal { "Dear " + name } else { "Hi " + name }
#let shadow(x) = {
  let x = x + 1
  let y = x * 2
  let x = y + 1
  x
}
#let join-words(words) = {
  let out = ""
  for w in words {
    out = out + w
  }
  out
}
#let classify(n) = if n < 0 { "neg" } else if n == 0 { "zero" } else { "pos" }
