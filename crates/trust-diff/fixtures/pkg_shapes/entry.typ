// 入口:组合子模块。bare / 列表 / rename / 通配全部覆盖。
#import "utils.typ"
#import "nested/format.typ": pad, dec, join as merge-str
#import "nested/consts.typ": *

#let area(w, h) = w * h
#let describe(name, w, h) = merge-str("-", (name, dec(w), dec(h)))
#let scale-area(w, h, k: 2) = area(w, h) * k
#let golden-ratio() = PHI
