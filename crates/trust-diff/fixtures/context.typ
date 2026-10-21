// context 表达式(M2.1):推迟求值的闭包,realize/layout 期带
// location/styles 求值。
#let ctx-size = context text.size
#let add(a, b) = a + b
#let ctx-call = context add(1, 2)
