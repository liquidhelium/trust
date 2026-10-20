// 工具:被 bare import(`utils` 成为模块值)。
#let clamp(x, lo, hi) = if x < lo { lo } else if x > hi { hi } else { x }
#let lerp(a, b, t) = a + (b - a) * t
