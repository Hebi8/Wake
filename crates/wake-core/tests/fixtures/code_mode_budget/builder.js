const changes = {"/home/alice/a.rs": bodyA, "/home/alice/b.rs": bodyB};
let patch = "*** Begin Patch\n";
for (const [file, change] of Object.entries(changes)) {
    patch += `*** Update File: ${file}\n@@\n`;
    patch += change.map(line => "+" + line).join("\n") + "\n";
}
patch += "*** End Patch";
tools.apply_patch(patch);
