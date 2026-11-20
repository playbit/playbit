// An AppKit-owned interface to two independent local engine embeddings.
// All writers are synchronous on the main thread; wake only queues a later turn.
#import <Cocoa/Cocoa.h>
#include <mach/mach_time.h>
#include <stdint.h>
#include <stdio.h>

extern void*   fixture_open(const char*, void (*)(void));
extern int32_t fixture_toggle(void*);
extern int32_t fixture_value(void*);
extern int32_t fixture_drain(void*);
extern void    fixture_close(void*);

@interface          DemoController : NSObject <NSApplicationDelegate>
@property NSWindow* window;
@property NSArray<NSButton*>* checks;
@property NSArray<NSTextField*>* statuses;
@property NSString*              profile;
@property BOOL                   scheduled;
@property BOOL                   closing;
- (void)schedule;
@end
static DemoController* controller;
static uint64_t        began;
static uint64_t        nanos(void) {
    static mach_timebase_info_data_t scale;
    if (!scale.denom)
        mach_timebase_info(&scale);
    return mach_continuous_time() * scale.numer / scale.denom;
}
static void wake(void) {
    [controller schedule];
}
static NSTextField* label(NSString* text, CGFloat size, NSFontWeight weight) {
    NSTextField* view = [NSTextField labelWithString:text];
    view.font = [NSFont systemFontOfSize:size weight:weight];
    return view;
}

@implementation DemoController {
    void*    _models[2];
    uint64_t _editing[2];
    BOOL     _failed[2];
}
- (void)schedule {
    NSAssert([NSThread isMainThread], @"This fixture has only owner-thread writers");
    if (_scheduled || _closing)
        return;
    _scheduled = YES;
    dispatch_async(dispatch_get_main_queue(), ^{
        self.scheduled = NO; // Clear first: a partial drain can rearm this turn.
        if (self.closing)
            return;
        for (NSUInteger i = 0; i < 2; ++i) {
            if (!self->_models[i])
                continue;
            int32_t count = fixture_drain(self->_models[i]);
            if (count < 0) {
                self->_failed[i] = YES;
                self.statuses[i].stringValue = @"Changes unavailable. Reopen this window.";
                self.checks[i].enabled = NO;
            } else if (count > 0 && !self->_failed[i]) {
                [self render:i];
            }
        }
    });
}
- (void)render:(NSUInteger)i {
    int32_t value = fixture_value(_models[i]);
    if (value < 0) {
        _failed[i] = YES;
        self.statuses[i].stringValue = @"Could not read this list. Reopen this window.";
        self.checks[i].enabled = NO;
        return;
    }
    self.checks[i].state = value ? NSControlStateValueOn : NSControlStateValueOff;
    self.checks[i].enabled = YES;
    self.statuses[i].stringValue = @"Saved";
    [self.window.contentView layoutSubtreeIfNeeded];
    if (_editing[i]) {
        uint64_t elapsed = nanos() - _editing[i];
        _editing[i] = 0;
        fprintf(
            stdout,
            "{\"event\":\"edit\",\"list\":%lu,\"value\":%d,\"ready_ns\":%llu}\n",
            (unsigned long)i,
            value,
            (unsigned long long)elapsed);
        fflush(stdout);
    }
}
- (void)toggle:(NSButton*)sender {
    NSUInteger i = (NSUInteger)sender.tag;
    _editing[i] = nanos();
    sender.enabled = NO; // One outstanding UI turn per embedded owner.
    self.statuses[i].stringValue = @"Saving…";
    if (fixture_toggle(_models[i]) != 0) {
        // Failure may follow publication: read current state, never imply rollback.
        int32_t value = fixture_value(_models[i]);
        if (value >= 0)
            sender.state = value ? NSControlStateValueOn : NSControlStateValueOff;
        _failed[i] = YES;
        self.statuses[i].stringValue = @"Could not save. Reopen this window.";
    }
    // The committed-change wake owns successful readback and UI completion.
}
- (BOOL)openListsInto:(NSMutableArray<NSView*>*)views {
    NSMutableArray<NSButton*>*    checks = [NSMutableArray array];
    NSMutableArray<NSTextField*>* statuses = [NSMutableArray array];
    for (NSUInteger i = 0; i < 2; ++i) {
        NSString* path = [self.profile
            stringByAppendingPathComponent:i == 0 ? @"first" : @"second"];
        _models[i] = fixture_open(path.fileSystemRepresentation, wake);
        if (!_models[i]) {
            NSAlert* alert = [NSAlert new];
            alert.messageText = @"Could not open the lists";
            alert.informativeText =
                @"Use a separate folder for this example. Existing data was not reset.";
            [alert runModal];
            [NSApp terminate:nil];
            return NO;
        }
        NSButton* check = [NSButton
            checkboxWithTitle:i == 0 ? @"Plan a walk" : @"Read a chapter"
                       target:self
                       action:@selector(toggle:)];
        check.tag = (NSInteger)i;
        int32_t value = fixture_value(_models[i]);
        check.state = value == 1 ? NSControlStateValueOn : NSControlStateValueOff;
        check.enabled = value >= 0;
        NSTextField* status = label(
            value >= 0 ? @"Saved" : @"Could not read this list", 12, NSFontWeightRegular);
        status.textColor = NSColor.secondaryLabelColor;
        NSStackView* row = [NSStackView stackViewWithViews:@[check, status]];
        row.orientation = NSUserInterfaceLayoutOrientationVertical;
        row.alignment = NSLayoutAttributeLeading;
        row.spacing = 5;
        [views addObject:row];
        [checks addObject:check];
        [statuses addObject:status];
    }
    self.checks = checks;
    self.statuses = statuses;
    return YES;
}
- (void)applicationDidFinishLaunching:(NSNotification*)notification {
    (void)notification;
    NSMenu*     menu = [NSMenu new];
    NSMenuItem* item = [NSMenuItem new];
    [menu addItem:item];
    NSMenu* application = [NSMenu new];
    [application addItemWithTitle:@"Quit" action:@selector(terminate:) keyEquivalent:@"q"];
    item.submenu = application;
    NSApp.mainMenu = menu;
    _window = [[NSWindow alloc]
        initWithContentRect:NSMakeRect(0, 0, 520, 300)
                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
                            | NSWindowStyleMaskMiniaturizable
                    backing:NSBackingStoreBuffered
                      defer:NO];
    _window.title = @"Local Lists";
    _window.releasedWhenClosed = NO;
    NSMutableArray<NSView*>* views = [NSMutableArray array];
    [views addObject:label(@"Two lists, saved locally", 24, NSFontWeightSemibold)];
    NSTextField* subtitle = label(@"Each list keeps its own changes.", 13, NSFontWeightRegular);
    subtitle.textColor = NSColor.secondaryLabelColor;
    [views addObject:subtitle];
    if (![self openListsInto:views])
        return;
    NSStackView* stack = [NSStackView stackViewWithViews:views];
    stack.orientation = NSUserInterfaceLayoutOrientationVertical;
    stack.alignment = NSLayoutAttributeLeading;
    stack.spacing = 22;
    stack.translatesAutoresizingMaskIntoConstraints = NO;
    [_window.contentView addSubview:stack];
    [NSLayoutConstraint activateConstraints:@[
        [stack.leadingAnchor constraintEqualToAnchor:_window.contentView.leadingAnchor constant:28],
        [stack.trailingAnchor
            constraintEqualToAnchor:_window.contentView.trailingAnchor
                           constant:-28],
        [stack.topAnchor constraintEqualToAnchor:_window.contentView.topAnchor constant:26],
    ]];
    [_window center];
    [_window makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    [_window.contentView layoutSubtreeIfNeeded];
    [_window displayIfNeeded];
    fprintf(stdout,
            "{\"event\":\"startup\",\"ready_ns\":%llu}\n",
            (unsigned long long)(nanos() - began));
    fflush(stdout);
}
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication*)sender {
    (void)sender;
    return YES;
}
- (void)applicationWillTerminate:(NSNotification*)notification {
    (void)notification;
    self.closing = YES;
    for (NSUInteger i = 0; i < 2; ++i) {
        fixture_close(_models[i]);
        _models[i] = NULL;
    }
}
@end

int main(int argc, const char** argv) {
    began = nanos();
    if (argc != 2 || argv[1][0] != '/') {
        fprintf(stderr, "usage: engine-ui ABSOLUTE_DEDICATED_PROFILE\n");
        return 2;
    }
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        controller = [DemoController new];
        controller.profile = [NSString stringWithUTF8String:argv[1]];
        NSApp.delegate = controller;
        [NSApp run];
    }
    return 0;
}
