// Regression anchors for the differential tests: wrong-value/crash-level
// fixes and error-path messages. Each binding anchors one fix; the call
// cases live in cases("regress") in tests/diff.rs.

// Stateful function with a destructuring parameter (the stub signature must
// bind the synthesized name for forwarding to work).
#let base = 10
#let pat((a, b), k: 2) = a + b * base + k

// Multi-segment assignment: a missing final field is created; a missing
// intermediate key errors (error case).
#let deep() = {
  let d = (a: (b: 1))
  d.a.c = 5
  d
}
#let mid-missing() = {
  let d = (a: 1)
  d.x.y = 2
  d
}
#let compound-missing() = {
  let d = (a: 1)
  d.zz += 1
  d
}

// The value of a bounded while loop; two error paths: a statically true
// condition and the iteration guardrail.
#let count-to(n) = {
  let i = 0
  while i < n { i += 1 }
  i
}
#let always-true() = { while true {} }
#let spin() = { let n = 0; while n < 5 { n = 0 } }

// Code-block value = the join of all statement values (string
// concatenation / none is transparent; unjoinable values error).
#let block-join() = { "a"; "b"; "c" }
#let block-none() = { none; 1 }
#let block-tail-none() = { 1; none }
#let block-join-fail() = { 1; 2 }

// Named-argument defaults evaluate once (a missing argument takes the
// default).
#let pow2 = 3
#let def-ok(x: 2 * pow2) = x

// The callee is not callable (cast message; arguments are not evaluated).
#let not-fn = 5

// Dict-key method calls / the two-stage no-such-method error (error case).
#let mk() = (a: 1)

// Self-recursion reaching the depth guardrail (typst closures capture at
// creation time and top-level mutual references are not visible, so
// self-recursion exercises the call-depth guardrail through the stub).
#let down(n) = if n == 0 { 0 } else { down(n - 1) }

// Style-chain lookup of an element function's field (inside context).
#let size-of() = context text.size

// Equations: math is interpreted from a source snippet (the only category
// that goes through eval; two shapes — symbols and outer-variable
// captures).
#let eq-simple() = $1 + 1$
#let eq-sym() = $arrow$
#let eq-captured(x) = $x + 1$

// Value-position closure (bodies over 32 nodes go native): multiple
// captures + mixed arithmetic.
#let base-scale = 1pt
#let big-closure(a, b) = {
  let f = (x) => {
    let acc = a * x + b
    let res = acc * 2 + b - a
    let more = res + acc - b + a
    let s1 = more * 3 + acc - res + b * b - a
    let s2 = s1 + more - acc + res * 2 - b + a
    s2 + s1 + more + acc + res + a + b
  }
  f(2) + f(3)
}

// Self-recursive closure: a named closure whose body references itself
// (native path, SelfFuncCell).
#let self-rec(n) = {
  let countdown(k) = {
    if k <= 0 { 0 } else if k == 1 { 1 } else { k + countdown(k - 1) }
  }
  countdown(n)
}
