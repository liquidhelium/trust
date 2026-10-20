// 标准库调用:math/calc/str/int、构造器与转换。
#let hypot(a, b) = calc.sqrt(a * a + b * b)
#let clamp01(x) = calc.clamp(x, 0, 1)
#let mins(nums) = calc.min(..nums)
#let pow-of-two(n) = calc.pow(2, n)
#let shout(s) = upper(str(s))
#let str-len(s) = str(s).len()
#let to-int(s) = int(s)
#let repr-ish(x) = str(x)
#let repeat-str(s, n) = s * n
#let abs-sum(nums) = nums.map(calc.abs).sum()
