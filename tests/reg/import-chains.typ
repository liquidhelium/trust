// 真实回归用例:模块值导入链的槽名统一(VIA 链名)。fletcher 形态——
// 裸导入模块值字段(`import cetz.draw`)、跨文件再导出、下游引用;
// 曾出现导出表短名与 glue 铸的 VIA 名不一致(产物编译失败)。

--- reg-module-value-bare-import eval ---
#import "outer.typ": cetzlib
#import cetzlib.draw
#test(draw.line(), 1)

--- reexport-chain-through-module-value eval ---
#import "reexport.typ": *
#test(cetzlib.draw.mark(), 2)

--- reg-module-value-list-import eval ---
#import "outer.typ": cetzlib
#import cetzlib: draw
#test(draw.mark(), 2)
