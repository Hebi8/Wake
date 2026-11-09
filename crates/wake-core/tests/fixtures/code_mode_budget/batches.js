const file = "你好😀.rs";
const patches = [`*** Begin Patch
*** Add File: /home/alice/${file}
+sample
*** End Patch`, "*** Begin Patch\n*** Update File: /home/alice/second.rs\n@@\n-a\n+b\n*** End Patch"];
for (const patch of patches) { const result = await tools.apply_patch(patch); text(result); }
