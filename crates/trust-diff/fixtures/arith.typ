// 算术与运算符:整型、浮点、字符串拼接、比较、逻辑、in。
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
