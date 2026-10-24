// Arithmetic and operators: integers, floats, string concatenation, comparison, logic, in.
#let add(a, b) = a + b
#let mul(a, b) = a * b
#let fdiv(a, b) = a / b
#let neg(x) = -x
#let concat(a, b) = a + b
#let max2(a, b) = if a >= b { a } else { b }
#let both(a, b) = a and b
#let either(a, b) = a or b
#let clamp(x, lo, hi) = if x < lo { lo } else if x > hi { hi } else { x }
#let contains-needle(hay, needle) = needle in hay
#let pt-of(x) = x * 1pt
#let deg-sum(a) = a + 45deg
#let half() = 50%
#let em-plus(x) = x + 2em
