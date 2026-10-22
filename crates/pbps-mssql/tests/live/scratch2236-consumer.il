.assembly extern mscorlib { .publickeytoken = (B7 7A 5C 56 19 34 E0 89) .ver 4:0:0:0 }
.assembly extern Pbps2236Provider { .ver 1:0:0:0 }
.assembly Pbps2236Consumer { .ver 1:0:0:0 }
.module Pbps2236Consumer.dll
.class public auto ansi Consumer extends [mscorlib]System.Object {
 .method public hidebysig static int32 Value() cil managed {
  .maxstack 1
  call int32 [Pbps2236Provider]Provider::Value()
  ret
 }
}
