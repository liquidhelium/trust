// Context expressions: deferred-evaluation closures, evaluated during
// realize/layout with location/styles.
#let ctx-size = context text.size
#let add(a, b) = a + b
#let ctx-call = context add(1, 2)
