// Entry point: composes the submodules. bare / list / rename / wildcard
// imports are all covered.
#import "utils.typ"
#import "nested/format.typ": pad, dec, join as merge-str
#import "nested/consts.typ": *

#let area(w, h) = w * h
#let describe(name, w, h) = merge-str("-", (name, dec(w), dec(h)))
#let scale-area(w, h, k: 2) = area(w, h) * k
#let golden-ratio() = PHI
