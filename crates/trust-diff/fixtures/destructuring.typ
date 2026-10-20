// 解构:数组/字典模式、spread 剩余、for 解构、解构赋值。
#let fst-snd(pair) = {
  let (a, b) = pair
  a + b
}
#let swap-xy(point) = {
  let (x, y) = point
  (y, x)
}
#let head-tail(arr) = {
  let (first, ..rest) = arr
  (first, rest)
}
#let dict-pick(d) = {
  let (a, b: second) = d
  a * second
}
#let dict-rest(d) = {
  let (a, ..others) = d
  others.len()
}
#let sum-pairs(pairs) = {
  let total = 0
  for (a, b) in pairs {
    total += a * b
  }
  total
}
#let reassign(pair) = {
  let (a, b) = pair
  (a, b) = (b, a)
  a - b
}
#let nested(v) = {
  let ((a, b), c) = v
  a + b + c
}
