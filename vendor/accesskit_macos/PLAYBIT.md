# AccessKit macOS adapter

Source: accesskit_macos 0.27.0 from crates.io, upstream commit
42e53b0d829e7a0b34dc8803bb06012db2e80cc6, MIT OR Apache-2.0.
This local patch preserves macOS checkbox accessibility values 0/1/2 for
unchecked/checked/mixed. The published version reduces mixed to boolean true.
See Apple's AX checkbox convention as exercised by WebKit:
https://bugs.webkit.org/show_bug.cgi?id=158876

A regression test covers the three values through the native adapter wrapper.
All other adapter code is unchanged. GPUI owns projection and action dispatch;
this adapter only provides native accessibility peers and notifications.
