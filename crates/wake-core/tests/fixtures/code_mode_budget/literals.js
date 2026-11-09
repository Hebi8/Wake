const patch = "*** Begin Patch\n*** Add File: /home/alice/first.rs\n+const text = \"sample\";\n*** End Patch";
text(await tools.apply_patch(patch));
tools.apply_patch('*** Begin Patch\n*** Delete File: /home/alice/old.rs\n*** End Patch');
