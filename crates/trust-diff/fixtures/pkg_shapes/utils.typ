// Utilities: imported bare (`utils` becomes a module value).
#let clamp(x, lo, hi) = if x < lo { lo } else if x > hi { hi } else { x }
#let lerp(a, b, t) = a + (b - a) * t
