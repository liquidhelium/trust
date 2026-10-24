// Control flow and recursion: if chains, while, for joins, local bindings and shadowing.
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
#let first-over(nums, limit) = {
  for n in nums {
    if n > limit { break }
    if calc.even(n) { continue }
  }
  "done"
}
#let sum-odds(nums) = {
  let total = 0
  for n in nums {
    if calc.even(n) { continue }
    total = total + n
  }
  total
}
#let skip-to(nums, target) = {
  let out = ()
  for n in nums {
    if n == target { break }
    out.push(n)
  }
  out
}
#let find-first(nums, pred) = {
  for n in nums {
    if pred(n) { return n }
  }
  -1
}
#let accumulate(nums) = {
  let total = 0
  for n in nums {
    total += n
  }
  total
}
// Error path: the condition must be a boolean (message mirrors rt::boolean and typst-eval).
#let bool-fail() = if 1 { "t" }
