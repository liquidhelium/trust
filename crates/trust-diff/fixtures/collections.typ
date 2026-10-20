// 集合:数组/字典构造、spread、方法调用、成员判断。
#let make-range(n) = range(n)
#let total(nums) = nums.sum()
#let doubled(nums) = nums.map(x => x * 2)
#let evens(nums) = nums.filter(x => calc.even(x))
#let sorted-desc(nums) = nums.sorted(key: x => -x)
#let at-or(nums, i, fallback: -1) = nums.at(i, default: fallback)
#let count-in(item, arr) = if item in arr { true } else { false }
#let dict-make(a, b) = (a: a, b: b, "c-key": 3)
#let dict-get(d) = d.a + d.at("b")
#let dict-keys(d) = d.keys()
#let dict-spread(base) = (..base, extra: 42)
#let pair-up(names, values) = {
  let out = ()
  for i in range(names.len()) {
    out.push((names.at(i), values.at(i)))
  }
  out
}
#let merge-all(dicts) = {
  let out = (:)
  for d in dicts {
    out = (: ..out, ..d)
  }
  out
}
