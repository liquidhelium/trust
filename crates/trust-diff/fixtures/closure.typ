// Closures: captures, higher-order functions, default parameters, sinks, named arguments.
#let adder = (n) => (x) => x + n
#let compose = (f, g) => (x) => f(g(x))
#let apply-twice(f, x) = f(f(x))
#let greet2(name, greeting: "Hello") = greeting + ", " + name
#let variadic(first, ..rest) = (first, rest.pos(), rest.named())
#let kwargs(..args) = args
#let fold-nums(nums, init, f) = nums.fold(init, f)
#let zip-with(f, a, b) = a.zip(b).map(p => f(p.at(0), p.at(1)))
