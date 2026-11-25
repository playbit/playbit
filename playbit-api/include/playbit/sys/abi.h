#ifndef PLAYBIT_SYS_ABI_H
#define PLAYBIT_SYS_ABI_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

// Typed operation wrappers over the scalar syscall ABI. Native pointers refer
// directly to caller storage; Wasm pointers are validated linear-memory offsets.
typedef struct PBContext PBContext;
typedef int32_t          PBStatus;
typedef int64_t          PBSysResult;
enum {
    PB_OK = 0,
    PB_NOT_FOUND = -1,
    PB_ACCESS_DENIED = -2,
    PB_INVALID_ARGUMENT = -3,
    PB_INTERNAL = -4,
    PB_UNAVAILABLE = -5,
    PB_WOULD_BLOCK = -6,
    PB_UNSUPPORTED = -7,
    PB_STALE_HANDLE = -8,
    PB_LIMIT_EXCEEDED = -9,
    PB_CONFLICT = -10,
    PB_BUFFER_TOO_SMALL = -11,
    PB_BUSY = -12,
    PB_CLOSED = -13,
    PB_DESTINATION_DELETED = -14,
    PB_UNCERTAIN = -15
};
// Unknown opcodes return PB_UNSUPPORTED. Unused argument slots are zero.
// Direct-handle operations pack slot (low u32) and generation (high 31 bits) in a.
#ifdef __wasm32__
__attribute__((import_module("playbit"), import_name("pb_syscall"))) int64_t pb_syscall(
    uint32_t operation, uint64_t a, uint64_t b, uint64_t c, uint64_t d, uint64_t e, uint64_t f);
static inline PBContext* pb_guest_context(void) {
    return (PBContext*)(uintptr_t)1;
}
#else
int64_t pb_syscall(
    PBContext* context,
    uint32_t   operation,
    uint64_t   a,
    uint64_t   b,
    uint64_t   c,
    uint64_t   d,
    uint64_t   e,
    uint64_t   f);
#endif
static inline uint64_t pb_handle_pack(uint32_t generation, uint32_t slot) {
    return ((uint64_t)generation << 32) | (uint64_t)slot;
}
static inline int64_t pb_syscall_invoke(
    PBContext* context,
    uint32_t   operation,
    uint64_t   a,
    uint64_t   b,
    uint64_t   c,
    uint64_t   d,
    uint64_t   e,
    uint64_t   f) {
#ifdef __wasm32__
    if (context != pb_guest_context())
        return PB_INVALID_ARGUMENT;
    return pb_syscall(operation, a, b, c, d, e, f);
#else
    return pb_syscall(context, operation, a, b, c, d, e, f);
#endif
}

// Audio and sound handles are capabilities private to the current guest table.
// PCM16 little-endian samples are interleaved by frame, mono or left/right stereo.
// Sound creation copies frame_count * channel_count * 2 bytes and returns its
// positive handle directly. At most 16 sounds and 1 MiB total
// PCM data, including storage awaiting callback reclamation, belong to one app.
typedef uint64_t PBOperationHandle;
typedef uint64_t PBAudio;
typedef uint64_t PBSound;
typedef struct PBSoundInstance {
    PBSound sound;
    uint32_t instance;
} PBSoundInstance;
enum {
    PB_AUDIO_DEFAULT_SAMPLE_RATE = 48000,
    PB_AUDIO_MIN_SAMPLE_RATE = 8000,
    PB_AUDIO_MAX_SAMPLE_RATE = 192000,
    PB_AUDIO_MAX_CHANNELS = 2,
    PB_AUDIO_MAX_PLAYBACK_INSTANCES = 8,
    PB_AUDIO_UNITY_GAIN = 65536,
    PB_SOUND_LOOP = 1
};
static inline uint32_t pb_audio_float_bits(float value) {
    uint32_t bits;
    // Character accesses copy the representation without aliasing or libc.
    const unsigned char* source = (const unsigned char*)&value;
    unsigned char* destination = (unsigned char*)&bits;
    for (uint32_t i = 0; i < sizeof(bits); i++)
        destination[i] = source[i];
    return bits;
}
static inline PBSysResult pb_audio_open_begin(PBContext* context, uint32_t preferred_sample_rate) {
    return pb_syscall_invoke(context, 0x1a0, preferred_sample_rate, 0, 0, 0, 0, 0);
}
// PB_WOULD_BLOCK retains the operation. Terminal completion consumes it.
static inline PBSysResult pb_audio_open_take(PBContext* context, PBOperationHandle operation) {
    return pb_syscall_invoke(context, 0x1a1, operation, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_audio_close(PBContext* context, PBAudio audio) {
    return pb_syscall_invoke(context, 0x1a2, audio, 0, 0, 0, 0, 0);
}
// Returns the negotiated sample rate, or a negative PBStatus.
static inline PBSysResult pb_audio_sample_rate(PBContext* context, PBAudio audio) {
    return pb_syscall_invoke(context, 0x1a3, audio, 0, 0, 0, 0, 0);
}
// Returns float bits in the low u32, or a negative PBStatus. Volume is [0,1].
static inline PBSysResult pb_audio_volume(PBContext* context, PBAudio audio) {
    return pb_syscall_invoke(context, 0x1a4, audio, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_audio_set_volume(PBContext* context, PBAudio audio, float volume) {
    return pb_syscall_invoke(context, 0x1a5, audio, pb_audio_float_bits(volume), 0, 0, 0, 0);
}
static inline PBSysResult pb_sound_open_pcm(PBContext* context, PBAudio audio, uint32_t channel_count, uint32_t sample_rate, const uint8_t* samples, uint32_t frame_count) {
    return pb_syscall_invoke(context, 0x1a6, audio, channel_count, sample_rate, (uintptr_t)samples, frame_count, 0);
}
static inline PBSysResult pb_sound_close(PBContext* context, PBSound sound) {
    return pb_syscall_invoke(context, 0x1a7, sound, 0, 0, 0, 0, 0);
}
// Each (sound,instance) is independent. Replaying a pair replaces its playback.
// Gain is an amplitude multiplier divided by 65536. Start/length are source
// nanoseconds rounded down to PCM frames. Length zero is invalid. Flags may
// contain PB_SOUND_LOOP. Admission can return PB_LIMIT_EXCEEDED without playing.
static inline PBSysResult pb_sound_play(PBContext* context, PBSoundInstance sound, uint32_t gain, uint64_t start, uint64_t length, uint32_t flags) {
    return pb_syscall_invoke(context, 0x1a8, sound.sound, sound.instance, gain, start, length, flags);
}
// Renderer position in source nanoseconds, rounded up so seek(tell()) restores
// the source frame. This does not include device latency.
static inline PBSysResult pb_sound_tell(PBContext* context, PBSoundInstance sound, uint64_t* output) {
    return pb_syscall_invoke(context, 0x1a9, sound.sound, sound.instance, (uintptr_t)output, 0, 0, 0);
}
static inline PBSysResult pb_sound_seek(PBContext* context, PBSoundInstance sound, uint64_t position) {
    return pb_syscall_invoke(context, 0x1aa, sound.sound, sound.instance, position, 0, 0, 0);
}
static inline PBSysResult pb_sound_set_gain(PBContext* context, PBSoundInstance sound, uint32_t gain) {
    return pb_syscall_invoke(context, 0x1ab, sound.sound, sound.instance, gain, 0, 0, 0);
}
// Balance [-1,1]: -1 left, 0 unchanged stereo, 1 right. Only attenuates channels.
static inline PBSysResult pb_sound_set_balance(PBContext* context, PBSoundInstance sound, float balance) {
    return pb_syscall_invoke(context, 0x1ac, sound.sound, sound.instance, pb_audio_float_bits(balance), 0, 0, 0);
}
// Stop destroys the playback instance, including one that has reached its end.
static inline PBSysResult pb_sound_stop(PBContext* context, PBSoundInstance sound) {
    return pb_syscall_invoke(context, 0x1ad, sound.sound, sound.instance, 0, 0, 0, 0);
}

// Only future advisory event IDs may carry this bit; existing IDs remain mandatory.
#define PB_OPTIONAL_EVENT_KIND_BIT UINT32_C(0x80000000)

enum {
    PB_EVENT_STARTED = 1,
    PB_EVENT_CLICK = 2,
    PB_EVENT_SUBMIT = 3,
    PB_EVENT_DATA_CHANGED = 4,
    PB_EVENT_PRESENCE_CHANGED = 5,
    PB_EVENT_UPDATE_AVAILABLE = 6,
    PB_EVENT_OBJECT_CREATION_READY = 7,
    PB_EVENT_FILE_COPY_READY = 8,
    PB_EVENT_FILE_SELECTION_READY = 9,
    PB_EVENT_WAKE = 10,
    // Original named-canvas pointer. Four little-endian u32 values (16 bytes)
    // for canvas x, canvas y, phase (move=0, down=1, up=2, cancel=3), and
    // primary button state (0 or 1). Target is the canvas ID.
    PB_EVENT_POINTER = 11,
    // Original named-canvas key. Two little-endian u32 values (8 bytes)
    // for physical key (left=1, right=2, up=3, down=4, space=5) and pressed
    // state (0 or 1). Target is the canvas ID.
    PB_EVENT_KEY = 12,
    PB_EVENT_FRAME = 13,
    // Payload: u32 edit_count, packed {u32 start,end,inserted_len} edits, UTF-8 value.
    // Offsets and lengths are bytes. Count UINT32_MAX means unavailable ranges.
    // Value <=262144 bytes, count <=256, complete payload <=265220 bytes.
    PB_EVENT_TEXT_CHANGED = 14,
    PB_EVENT_SURFACE_CHANGED = 15,
    PB_EVENT_DATA_READY = 16,
    PB_EVENT_VOLUMES_CHANGED = 17, // target=0, little-endian uint64_t revision payload
    PB_EVENT_CHANNEL_MESSAGE = 18, // handle (8), participant (80), message ID (16), bytes
    PB_EVENT_CHANNEL_STATUS = 19, // target=0, little-endian int32_t status
    // PBPointerInput (56 bytes) or PBPenInput (92 bytes) for PB_POINTER_PEN.
    // Target is the named canvas ID when applicable, or zero.
    PB_EVENT_WINDOW_POINTER = 22,
    // PBKeyboardInput (60 bytes). Target is zero.
    PB_EVENT_WINDOW_KEYBOARD = 23,
    // PBScrollInput (44 bytes). Target is zero.
    PB_EVENT_WINDOW_SCROLL = 24,
    // PBGestureInput (48 bytes). Target is zero.
    PB_EVENT_WINDOW_GESTURE = 25,
    // PBGamepadInput (36 bytes). Target is zero.
    PB_EVENT_WINDOW_GAMEPAD = 26,
};
// Reread identity.read after this advisory event (target zero, empty payload).
#define PB_EVENT_IDENTITY_CHANGED UINT32_C(0x80000015)
// Host identity is independent of the app compilation target. Web means
// the web host, including Safari on macOS/iOS. Platform identity does not
// describe the surface, input facilities or resource envelope.
enum {
    PB_PLATFORM_OTHER = 0,
    PB_PLATFORM_MACOS = 1,
    PB_PLATFORM_IOS = 2,
    PB_PLATFORM_WEB = 3
};
// Returns a PB_PLATFORM_* value or a negative PBStatus; no output allocation.
static inline PBSysResult pb_host_platform(PBContext* context) {
    return pb_syscall_invoke(context, 0xc, 0, 0, 0, 0, 0, 0);
}

// Fixed per-instance resource ceilings. Discover operation support separately.
// Unknown limit kinds return PB_UNSUPPORTED.
enum {
    PB_LIMIT_QUERY_RECORDS = 1,
    // Logical value bytes: text/bytes lengths, bool=1, integer/float=8,
    // reference=32, file metadata=40. Excludes record framing and scratch.
    PB_LIMIT_QUERY_VALUE_BYTES = 2,
    // Complete encoded ViewSubmit, including framing. Other UI limits also apply.
    PB_LIMIT_VIEW_BYTES = 3
};
// Returns a positive ceiling or a negative PBStatus, with no output allocation.
static inline PBSysResult pb_host_limit(PBContext* context, uint32_t limit) {
    return pb_syscall_invoke(context, 0xd, limit, 0, 0, 0, 0, 0);
}

// Immutable per-instance intent: 1 begins Quit when the last window closes;
// 0 retains the existing owner without windows. Other values are invalid;
// changing an already declared choice returns PB_CONFLICT. Local, with no I/O.
// Explicit user Quit always deactivates and drains accepted work.
static inline PBSysResult pb_quit_on_last_window(PBContext* context, uint32_t intent) {
    return pb_syscall_invoke(context, 0xe, intent, 0, 0, 0, 0, 0);
}

// Packed trace input: little-endian event headers, inline metadata, zero padding
// to 8 bytes. Concatenate events directly; total input <=65536 bytes per call.
// TraceConfig host event (kind 20) supplies generation u64 once per change at
// callback entry. Zero disables tracing. It never wakes an idle guest.
typedef uint64_t PBTraceTimestamp;
typedef uint32_t PBTraceThreadId;
enum { PB_TRACE_START=1, PB_TRACE_END=2, PB_TRACE_INSTANT=3 };
typedef struct PBTraceEvent {
    PBTraceTimestamp timestamp;
    PBTraceTimestamp reference_eid;
    uint64_t value;
    PBTraceThreadId reference_tid;
    uint32_t metadata_len;
    uint16_t name;
    uint8_t kind;
    uint8_t reserved[5];
} PBTraceEvent;
// Each header is followed by metadata_len UTF-8 JSON object bytes (<=4096),
// then zero padding. END references its START timestamp; guests use tid zero.
// Submission is consumed on every result; never retry it. Malformed batches
// publish nothing. Admission losses and stale capture tails remain incomplete.
// generation is the nonzero TraceConfig generation; lost is the cumulative
// producer loss count, never decreasing within that generation. A zero-length
// input can report loss alone. Remaining syscall slots are zero.
static inline PBSysResult pb_trace_record(PBContext* context, const uint8_t* bytes, uint32_t length, uint64_t generation, uint64_t lost) {
    return pb_syscall_invoke(context, 0xf, (uint64_t)(uintptr_t)bytes, length, generation, lost, 0, 0);
}
// General monotonic clock read. Nanoseconds, not wall time. Always succeeds.
static inline uint64_t pb_clock_read(PBContext* context) {
    return (uint64_t)pb_syscall_invoke(context, 0x10, 0, 0, 0, 0, 0, 0);
}

// Cryptographically secure platform randomness. Returns PB_OK after filling all
// bytes; entropy failure returns PB_UNAVAILABLE and zeroes the admitted output.
// Discard bytes on any error. Zero length succeeds without accessing bytes;
// a nonempty output must be writable and exclusively borrowed until return.
// Larger requests return PB_LIMIT_EXCEEDED before modifying the output.
enum { PB_MAX_RANDOM_BYTES = 65536 };
static inline PBSysResult pb_random_fill(PBContext* context, uint8_t* bytes, uint32_t length) {
    return pb_syscall_invoke(context, 0x14, (uint64_t)(uintptr_t)bytes, length, 0, 0, 0, 0);
}

enum { PB_INPUT_POINTER = 1, PB_INPUT_KEYBOARD = 2, PB_INPUT_TOUCH = 4 };
// SurfaceChanged: target=0, payload=16 bytes in this order, little-endian.
// Width and height are whole logical units. The default excludes host chrome
// and padding. A root canvas with Window extent receives the full drawable
// window. Zero dimensions mean no visible area.
// pixel_scale is finite positive IEEE 754 binary32, physical pixels/logical unit.
// inputs contains only PB_INPUT_* bits. Hosts notify initially and on change.
typedef struct PBSurface {
    uint32_t width, height;
    float    pixel_scale;
    uint32_t inputs;
} PBSurface;

// Every addressed data record starts with caller capacity and initialized extent.
// Initialize size to sizeof(record), written to zero, and all unused fields to zero.
// Hosts preserve size and unknown tails; only whole fields below written are valid.
// Arrays have caller stride; initialize each element's size before a result call.
// Input records may carry nonzero written returned by a prior successful call.
// Larger input records accept zero extension bytes only; nonzero tails are Unsupported.
enum { PB_RECORD_MAX_BYTES = 4096 };
#define PB_RECORD_INIT(type) { .size = (uint32_t)sizeof(type), .written = 0 }

// Key codes identify logical and physical keys. Key text uses Unicode scalars.
typedef enum PBKey {
    PB_KEY_None = 0,
    PB_KEY_Space = 1,
    PB_KEY_Quote = 2,
    PB_KEY_Comma = 3,
    PB_KEY_Minus = 4,
    PB_KEY_Period = 5,
    PB_KEY_Slash = 6,
    PB_KEY_0 = 7,
    PB_KEY_1 = 8,
    PB_KEY_2 = 9,
    PB_KEY_3 = 10,
    PB_KEY_4 = 11,
    PB_KEY_5 = 12,
    PB_KEY_6 = 13,
    PB_KEY_7 = 14,
    PB_KEY_8 = 15,
    PB_KEY_9 = 16,
    PB_KEY_Semicolon = 17,
    PB_KEY_Equal = 18,
    PB_KEY_A = 19,
    PB_KEY_B = 20,
    PB_KEY_C = 21,
    PB_KEY_D = 22,
    PB_KEY_E = 23,
    PB_KEY_F = 24,
    PB_KEY_G = 25,
    PB_KEY_H = 26,
    PB_KEY_I = 27,
    PB_KEY_J = 28,
    PB_KEY_K = 29,
    PB_KEY_L = 30,
    PB_KEY_M = 31,
    PB_KEY_N = 32,
    PB_KEY_O = 33,
    PB_KEY_P = 34,
    PB_KEY_Q = 35,
    PB_KEY_R = 36,
    PB_KEY_S = 37,
    PB_KEY_T = 38,
    PB_KEY_U = 39,
    PB_KEY_V = 40,
    PB_KEY_W = 41,
    PB_KEY_X = 42,
    PB_KEY_Y = 43,
    PB_KEY_Z = 44,
    PB_KEY_LeftBracket = 45,
    PB_KEY_Backslash = 46,
    PB_KEY_RightBracket = 47,
    PB_KEY_Grave = 48,
    PB_KEY_Escape = 49,
    PB_KEY_Enter = 50,
    PB_KEY_Tab = 51,
    PB_KEY_Backspace = 52,
    PB_KEY_Insert = 53,
    PB_KEY_Delete = 54,
    PB_KEY_Left = 55,
    PB_KEY_Right = 56,
    PB_KEY_Down = 57,
    PB_KEY_Up = 58,
    PB_KEY_PageUp = 59,
    PB_KEY_PageDown = 60,
    PB_KEY_Home = 61,
    PB_KEY_End = 62,
    PB_KEY_CapsLock = 63,
    PB_KEY_LeftShift = 64,
    PB_KEY_LeftCtrl = 65,
    PB_KEY_LeftAlt = 66,
    PB_KEY_LeftSuper = 67,
    PB_KEY_MediaNext = 68,
    PB_KEY_MediaPrev = 69,
    PB_KEY_MediaPlay = 70,
    PB_KEY_MediaStop = 71,
    PB_KEY_World1 = 72,
    PB_KEY_World2 = 73,
    PB_KEY_ScrollLock = 74,
    PB_KEY_NumLock = 75,
    PB_KEY_PrintScreen = 76,
    PB_KEY_Pause = 77,
    PB_KEY_F1 = 78,
    PB_KEY_F2 = 79,
    PB_KEY_F3 = 80,
    PB_KEY_F4 = 81,
    PB_KEY_F5 = 82,
    PB_KEY_F6 = 83,
    PB_KEY_F7 = 84,
    PB_KEY_F8 = 85,
    PB_KEY_F9 = 86,
    PB_KEY_F10 = 87,
    PB_KEY_F11 = 88,
    PB_KEY_F12 = 89,
    PB_KEY_F13 = 90,
    PB_KEY_F14 = 91,
    PB_KEY_F15 = 92,
    PB_KEY_F16 = 93,
    PB_KEY_F17 = 94,
    PB_KEY_F18 = 95,
    PB_KEY_F19 = 96,
    PB_KEY_F20 = 97,
    PB_KEY_F21 = 98,
    PB_KEY_F22 = 99,
    PB_KEY_F23 = 100,
    PB_KEY_F24 = 101,
    PB_KEY_Numpad0 = 102,
    PB_KEY_Numpad1 = 103,
    PB_KEY_Numpad2 = 104,
    PB_KEY_Numpad3 = 105,
    PB_KEY_Numpad4 = 106,
    PB_KEY_Numpad5 = 107,
    PB_KEY_Numpad6 = 108,
    PB_KEY_Numpad7 = 109,
    PB_KEY_Numpad8 = 110,
    PB_KEY_Numpad9 = 111,
    PB_KEY_NumpadDot = 112,
    PB_KEY_NumpadDivide = 113,
    PB_KEY_NumpadMultiply = 114,
    PB_KEY_NumpadSubtract = 115,
    PB_KEY_NumpadAdd = 116,
    PB_KEY_NumpadEnter = 117,
    PB_KEY_NumpadEquals = 118,
    PB_KEY_NumpadClear = 119,
    PB_KEY_RightShift = 120,
    PB_KEY_RightCtrl = 121,
    PB_KEY_RightAlt = 122,
    PB_KEY_RightSuper = 123,
    PB_KEY_Menu = 124,
    PB_KEY_VolumeUp = 125,
    PB_KEY_VolumeDown = 126,
    PB_KEY_Mute = 127,
    PB_KEY_Fn = 128,
    PB_KEY_COUNT = 129,
} PBKey;

// Window metadata command 29 takes one little-endian mask word.
enum {
    PB_WINDOW_INPUT_POINTER = 1u << 0,
    PB_WINDOW_INPUT_KEYBOARD = 1u << 1,
    PB_WINDOW_INPUT_SCROLL = 1u << 2,
    PB_WINDOW_INPUT_GESTURE = 1u << 3,
    PB_WINDOW_INPUT_GAMEPAD = 1u << 4
};
enum {
    PB_MOD_SHIFT = 1u << 0, PB_MOD_CTRL = 1u << 1, PB_MOD_ALT = 1u << 2,
    PB_MOD_META = 1u << 3, PB_MOD_CAPS_LOCK = 1u << 4, PB_MOD_FN = 1u << 5
};
enum { PB_POINTER_MOUSE = 1, PB_POINTER_TOUCH, PB_POINTER_TRACKPAD, PB_POINTER_PEN };
enum {
    PB_POINTER_PRIMARY = 1u << 0, PB_POINTER_IN_CONTACT = 1u << 1,
    PB_POINTER_ERASER = 1u << 2, PB_POINTER_INVERTED = 1u << 3,
    PB_POINTER_COALESCED = 1u << 4, PB_POINTER_PREDICTED = 1u << 5
};
enum {
    PB_POINTER_MOVE = 0, PB_POINTER_DOWN, PB_POINTER_UP, PB_POINTER_CANCEL,
    PB_POINTER_ENTER, PB_POINTER_LEAVE, PB_POINTER_NO_BUTTON = 255
};
enum {
    PB_BUTTON_LEFT = 0, PB_BUTTON_RIGHT, PB_BUTTON_MIDDLE, PB_BUTTON_X1, PB_BUTTON_X2,
    PB_BUTTON_COUNT = 16
};
enum { PB_INPUT_CHANGED = 0, PB_INPUT_BEGAN, PB_INPUT_ENDED, PB_SCROLL_MOMENTUM };
enum {
    PB_SCROLL_PRECISE = 1u << 0, PB_SCROLL_INVERTED = 1u << 1,
    PB_SCROLL_LINES = 1u << 2, PB_SCROLL_PAGES = 1u << 3
};
enum { PB_GESTURE_PAN = 0, PB_GESTURE_PINCH, PB_GESTURE_ROTATE };
enum {
    PB_GAMEPAD_CONNECTED = 0, PB_GAMEPAD_DISCONNECTED, PB_GAMEPAD_BUTTON_DOWN,
    PB_GAMEPAD_BUTTON_UP, PB_GAMEPAD_AXIS_MOVE
};
typedef enum PBGamepadButton {
    PB_GAMEPAD_A = 0, PB_GAMEPAD_B, PB_GAMEPAD_X, PB_GAMEPAD_Y,
    PB_GAMEPAD_LEFT_SHOULDER, PB_GAMEPAD_RIGHT_SHOULDER, PB_GAMEPAD_LEFT_TRIGGER_BUTTON,
    PB_GAMEPAD_RIGHT_TRIGGER_BUTTON, PB_GAMEPAD_LEFT_THUMBSTICK, PB_GAMEPAD_RIGHT_THUMBSTICK,
    PB_GAMEPAD_MENU, PB_GAMEPAD_OPTIONS, PB_GAMEPAD_HOME, PB_GAMEPAD_BUTTON_COUNT
} PBGamepadButton;
typedef enum PBGamepadAxis {
    PB_GAMEPAD_LEFT_X = 0, PB_GAMEPAD_LEFT_Y, PB_GAMEPAD_RIGHT_X, PB_GAMEPAD_RIGHT_Y,
    PB_GAMEPAD_DPAD_X, PB_GAMEPAD_DPAD_Y, PB_GAMEPAD_LEFT_TRIGGER_AXIS, PB_GAMEPAD_RIGHT_TRIGGER_AXIS,
    PB_GAMEPAD_AXIS_COUNT
} PBGamepadAxis;

// Every PB_EVENT_WINDOW_* payload starts with PBInputSource. Integers and
// IEEE 754 binary32 floats use little-endian byte order. Check the payload
// length returned by pb_event_poll and copy the bytes to an aligned value of
// the matching type before accessing it. The event header carries the window ID.
typedef struct PBInputSource {
    // Occurrence time in monotonic nanoseconds. Client ID zero means local.
    uint32_t timestamp_ns_lo, timestamp_ns_hi;
    uint32_t client_id, device_id;
    uint16_t modifiers, reserved; // PB_MOD_* bits and a zero word.
} PBInputSource;
static inline uint64_t pb_input_timestamp_ns(PBInputSource source) {
    return ((uint64_t)source.timestamp_ns_hi << 32) | source.timestamp_ns_lo;
}
typedef struct PBPointerInput {
    PBInputSource source;
    uint32_t pointer_id;
    uint8_t phase, kind, button, click_count;
    uint16_t flags, buttons;
    // Position and delta in logical units, with the origin at the content's
    // top left. Use canvas_x and canvas_y when the event targets a named canvas.
    float x, y, dx, dy, canvas_x, canvas_y;
} PBPointerInput;
typedef struct PBPenInput {
    PBPointerInput pointer;
    float pressure, tangential_pressure, tilt_x, tilt_y, twist;
    float width, height, altitude, azimuth;
} PBPenInput;
typedef struct PBKeyboardInput {
    PBInputSource source;
    uint16_t key, device_key; // Logical and physical PB_KEY_* codes.
    uint8_t pressed, repeat, text_len, reserved;
    uint32_t text[8]; // The first text_len entries are Unicode scalars.
} PBKeyboardInput;
typedef struct PBScrollInput {
    PBInputSource source;
    uint8_t kind, phase;
    uint16_t flags;
    float x, y, dx, dy, wheel_z; // PB_SCROLL_* flags describe delta units.
} PBScrollInput;
typedef struct PBGestureInput {
    PBInputSource source;
    uint8_t kind, phase;
    uint16_t reserved;
    float x, y, dx, dy, scale, rotation; // Relative scale and clockwise radians.
} PBGestureInput;
typedef struct PBGamepadInput {
    PBInputSource source;
    uint8_t phase, reserved;
    uint16_t control; // PBGamepadButton or PBGamepadAxis, according to phase.
    float value, x, y;
} PBGamepadInput;

typedef struct PBEvent {
    uint32_t size;
    uint32_t written;
    uint64_t generation;
    uint32_t kind;
    uint32_t target;
    uint32_t object_id; // Originating window, or zero if unknown.
} PBEvent;
enum { PB_DISPLAY_NAME_BYTES = 256 };
typedef struct PBIdentity {
    uint32_t size;
    uint32_t written;
    uint64_t generation;
    uint8_t account_id[16];
    uint8_t device_id[16];
    uint8_t instance_id[16];
    uint8_t session_id[32];
    uint32_t flags; // bit 0: service, bit 1: session present; other bits zero
    uint32_t reserved;
    // Zero-padded UTF-8, possibly full-width. Empty when unavailable.
    // A capacity ending inside this field leaves every byte of the field untouched.
    uint8_t display_name[PB_DISPLAY_NAME_BYTES];
} PBIdentity;

enum { PB_SESSION_HAS_KEY_SECRET = 1, PB_SESSION_CODE_CHARS = 22 };
typedef struct PBSessionInfo {
    uint32_t size;
    uint32_t written;
    uint32_t flags;
    uint32_t reserved;
    uint8_t  internal_id[32]; // The same public session ID as PBIdentity.session_id.
    uint8_t  key_secret[16];  // Invitation seed when PB_SESSION_HAS_KEY_SECRET is set.
} PBSessionInfo;

// Format a successfully read session's invitation. The caller lends 23 writable chars.
// Returns 22 and a NUL-terminated code, or 0 and an empty string without a key secret.
static inline uint32_t pb_session_code(
    const PBSessionInfo* info, char output[PB_SESSION_CODE_CHARS + 1]) {
    if (info->written < 64 || !(info->flags & PB_SESSION_HAS_KEY_SECRET)) {
        output[0] = '\0';
        return 0;
    }
    static const char alphabet[] = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    uint8_t digits[PB_SESSION_CODE_CHARS] = {0};
    for (uint32_t i = 0; i < sizeof(info->key_secret); i++) {
        uint32_t carry = info->key_secret[i];
        for (uint32_t j = PB_SESSION_CODE_CHARS; j > 0; j--) {
            uint32_t value = digits[j - 1] * 256u + carry;
            digits[j - 1] = (uint8_t)(value % 58);
            carry = value / 58;
        }
    }
    for (uint32_t i = 0; i < PB_SESSION_CODE_CHARS; i++)
        output[i] = alphabet[digits[i]];
    output[PB_SESSION_CODE_CHARS] = '\0';
    return PB_SESSION_CODE_CHARS;
}

// All callbacks execute on the owning thread. Context is borrowed during init
// and turn only: never free it or use it after instance destruction. Calls from
// another thread or outside init/turn return PB_CONFLICT. Callbacks must return
// cooperatively and must not unwind. Native apps share the host address space;
// they have no VM fuel isolation. The host invokes no callback under a syscall
// lock. init is called once on the first turn. *state starts NULL; ownership of
// its final value transfers to the host even when init reports failure.
// destroy is called exactly once after any init invocation, including failure;
// it releases app state and must not issue syscalls. user_data must remain valid
// through destroy. All three callback pointers are required.
typedef struct PBApp {
    uint32_t size;
    uint32_t written;
    PBStatus (*init)(PBContext*, void* user_data, void** state);
    PBStatus (*turn)(PBContext*, void* state);
    void (*destroy)(void* state);
    void* user_data;
} PBApp;

#ifndef __wasm32__
// Fill a caller-sized callback table; short output returns PB_BUFFER_TOO_SMALL.
// Preserve size and unknown tail bytes. Each init creates independent app state.
PBStatus pb_app(PBApp* output);
#endif

typedef uint8_t PBId[16];
typedef uint64_t PBFileHandle;
typedef uint64_t PBFileListHandle;
typedef uint64_t PBVolumeHandle;
// Stable identity inside the volume capability; each use rechecks entry authority.
typedef struct PBEntryHandle {
    uint32_t size;
    uint32_t written;
    uint64_t volume;
    uint8_t entry_id[15];
    uint8_t reserved;
} PBEntryHandle;
// Offsets/counts index the corresponding caller-owned array, never host memory.
typedef struct PBRange {
    uint32_t size;
    uint32_t written;
    uint32_t offset;
    uint32_t count;
} PBRange;
enum {
    PB_VOLUME_ROOT = 1,
    PB_VOLUME_BACKING_MASK = 0xff00,
    PB_VOLUME_PLAYBIT = 0x100,
    PB_VOLUME_HOST_FS = 0x200,
    PB_VOLUME_BUNDLE = 0x300
};
enum {
    PB_VOLUME_ROLE_NONE = 0,
    PB_VOLUME_ROLE_ROOT = 1,
    PB_VOLUME_ROLE_SESSION = 2,
    PB_VOLUME_ROLE_USER = 3,
    PB_VOLUME_ROLE_USER_SESSION = 4,
    PB_VOLUME_ROLE_LOCAL = 5,
    PB_VOLUME_ROLE_LOCAL_SESSION = 6
};
typedef struct PBVolume {
    uint32_t size;
    uint32_t written;
    uint64_t handle;
    uint8_t id[16];
    uint32_t name_offset;
    uint32_t name_count;
    uint32_t tags_offset;
    uint32_t tags_count;
    uint32_t rights; // read=1, write=2, schema=4
    uint32_t flags; // PB_VOLUME_*
    uint64_t file; // Fresh independent root; dispose when no longer needed.
    uint8_t grant_id[16]; // Opaque identity; conveys no authority.
    uint32_t role; // PB_VOLUME_ROLE_*; absent tail means NONE.
} PBVolume;
typedef struct PBVolumes {
    uint32_t size;
    uint32_t written;
    uint64_t revision;
    PBVolume* volumes_data;
    uint32_t volumes_capacity;
    uint32_t volumes_count;
    uint32_t volumes_stride;
    PBRange* tags_data;
    uint32_t tags_capacity;
    uint32_t tags_count;
    uint32_t tags_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBVolumes;
enum {
    PB_TEXT = 1,
    PB_BOOLEAN = 2,
    PB_INTEGER = 3,
    PB_FLOAT = 4,
    PB_BYTES = 5,
    PB_REFERENCE = 6,
    PB_FILE = 7
};
// Text/bytes borrow data[length]. Scalars: bool 0/1, signed integer bits, finite
// IEEE-754 double bits. Reference IDs: id1=volume, entry_id=database, id2=object.
// File references: id1=volume, entry_id=blob; id2 is zero. Unused members are zero.
// References convey identity and never grant access to their target.
typedef struct PBValueInput {
    uint32_t size;
    uint32_t written;
    uint32_t kind;
    uint32_t length;
    uint64_t scalar;
    uint8_t id1[16];
    uint8_t id2[16];
    uint8_t entry_id[15];
    uint8_t reserved;
    const uint8_t* data;
} PBValueInput;
typedef struct PBFieldInput {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    const PBValueInput* values;
    uint32_t count;
    uint32_t stride;
    uint32_t present;
} PBFieldInput;
typedef struct PBValue {
    uint32_t size;
    uint32_t written;
    uint32_t kind;
    uint32_t reserved;
    uint64_t scalar;
    uint8_t id1[16];
    uint8_t id2[16];
    uint8_t entry_id[15];
    uint8_t entry_reserved;
    uint32_t bytes_offset;
    uint32_t bytes_count;
} PBValue;
typedef struct PBField {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    uint32_t values_offset;
    uint32_t values_count;
    uint32_t present;
    uint32_t reserved;
} PBField;
typedef struct PBObject {
    uint32_t size;
    uint32_t written;
    uint8_t type_id[16];
    PBField* fields_data;
    uint32_t fields_capacity;
    uint32_t fields_count;
    uint32_t fields_stride;
    PBValue* values_data;
    uint32_t values_capacity;
    uint32_t values_count;
    uint32_t values_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBObject;

// Descriptor buffers must be initialized; input count members are ignored.
// PB_BUFFER_TOO_SMALL updates descriptor counts and the coherent volume revision;
// arrays/type_id remain unchanged. Retry reads a fresh authorized snapshot.
// Any other error leaves all outputs unchanged. Output arrays, descriptor and
// input ranges must be disjoint. NULL arrays require zero capacity.
// Limits: 256 volumes, 16384 tag ranges, 256 object fields, 4096 values, 1 MiB
// byte storage. Input values also share a 1 MiB logical payload limit.
static inline PBSysResult pb_volumes(PBContext* context, PBVolumes* output) {
    return pb_syscall_invoke(context, 0x7, (uint64_t)(uintptr_t)output, 0, 0, 0, 0, 0);
}

// One-shot notification relative to a successful enumeration. Replaces any
// previous registration and detects an already-newer root revision immediately.
static inline PBSysResult pb_volumes_observe(PBContext* context, uint64_t revision) {
    return pb_syscall_invoke(context, 18, revision, 0, 0, 0, 0, 0);
}

typedef struct PBSchemaFieldInput {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    const uint8_t* name_data;
    uint32_t name_length;
    uint32_t kind;
    uint32_t flags;
    uint8_t reference_type[16];
} PBSchemaFieldInput;
typedef struct PBSchemaTypeInput {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    const uint8_t* name_data;
    uint32_t name_length;
    const PBSchemaFieldInput* fields;
    uint32_t count;
    uint32_t stride;
    uint32_t flags;
} PBSchemaTypeInput;
typedef struct PBSchemaField {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    uint32_t name_offset;
    uint32_t name_count;
    uint32_t kind;
    uint32_t flags;
    uint8_t reference_type[16];
} PBSchemaField;
typedef struct PBSchemaType {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    uint32_t name_offset;
    uint32_t name_count;
    uint32_t fields_offset;
    uint32_t fields_count;
    uint32_t flags;
    uint32_t reserved;
} PBSchemaType;
typedef struct PBSchema {
    uint32_t size;
    uint32_t written;
    PBSchemaType* types_data;
    uint32_t types_capacity;
    uint32_t types_count;
    uint32_t types_stride;
    PBSchemaField* fields_data;
    uint32_t fields_capacity;
    uint32_t fields_count;
    uint32_t fields_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBSchema;
// Schema flags: deprecated=1 (types/fields), repeated=2 (fields only). Kinds
// reuse PB_TEXT..PB_FILE; only PB_REFERENCE has a nonzero reference_type.
// Limits: 256 types, 256 fields/type, 4096 total fields, 255 bytes/name and
// 1 MiB aggregate name bytes. Reserved/unused members must be zero.
// Fixed creation output must be non-NULL, writable and disjoint from every
// input, including nested value spans. Rejection performs no creation.
typedef struct PBQuery {
    uint32_t size;
    uint32_t written;
    uint8_t type_id[16];
    uint8_t after[16];
    uint32_t flags;
    uint32_t limit;
    const uint8_t (*fields)[16];
    uint32_t count;
    uint32_t reserved;
} PBQuery;
typedef struct PBObjectInfo {
    uint32_t size;
    uint32_t written;
    uint8_t id[16];
    uint8_t type_id[16];
    uint32_t fields_offset;
    uint32_t fields_count;
} PBObjectInfo;
typedef struct PBPage {
    uint32_t size;
    uint32_t written;
    uint8_t next[16];
    uint32_t has_next;
    uint32_t projected;
    PBObjectInfo* objects_data;
    uint32_t objects_capacity;
    uint32_t objects_count;
    uint32_t objects_stride;
    PBField* fields_data;
    uint32_t fields_capacity;
    uint32_t fields_count;
    uint32_t fields_stride;
    PBValue* values_data;
    uint32_t values_capacity;
    uint32_t values_count;
    uint32_t values_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBPage;
// Limit 1..64 objects; projection uses 1..256 field IDs when present. Absent
// after ID and projection count are zero. Page ranges index flat output arrays.
// Maximum storage capacities: 64 objects, 16384 fields, 211136 values, 1 MiB
// bytes (together below 16 MiB). Allocate only what the workload needs.
// Short output changes buffer counts only, preserving next/flags and arrays.

// Metadata results borrow no host storage. Ranges index bytes in the descriptor.
// Capacities: 256 capability names/32 KiB bytes, 32 arguments/32 KiB bytes,
// 1024 participants. All arrays and descriptors must be disjoint.
// Short outputs update only counts; arrays and other metadata remain unchanged.
// Other errors leave outputs unchanged. Every retry observes a fresh snapshot.
typedef struct PBCapabilities {
    uint32_t size;
    uint32_t written;
    PBRange* names_data;
    uint32_t names_capacity;
    uint32_t names_count;
    uint32_t names_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBCapabilities;
typedef struct PBArgument {
    uint32_t size;
    uint32_t written;
    uint32_t name_offset;
    uint32_t name_count;
    uint32_t value_offset;
    uint32_t value_count;
} PBArgument;
typedef struct PBArguments {
    uint32_t size;
    uint32_t written;
    PBArgument* arguments_data;
    uint32_t arguments_capacity;
    uint32_t arguments_count;
    uint32_t arguments_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBArguments;
typedef struct PBParticipant {
    uint32_t size;
    uint32_t written;
    uint8_t account_id[16];
    uint8_t device_id[16];
    uint8_t instance_id[16];
    uint8_t build_id[32];
    // Zero-padded UTF-8, possibly full-width. Empty when unavailable.
    // A capacity ending inside this field leaves every byte of the field untouched.
    uint8_t display_name[PB_DISPLAY_NAME_BYTES];
} PBParticipant;
static inline PBSysResult pb_capabilities(PBContext* context, PBCapabilities* output) {
    return pb_syscall_invoke(context, 0x1, (uint64_t)(uintptr_t)output, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_startup_arguments(PBContext* context, PBArguments* output) {
    return pb_syscall_invoke(context, 0x6, (uint64_t)(uintptr_t)output, 0, 0, 0, 0, 0);
}
// Read the cached roster. Call pb_session at startup for live membership and read
// again on each PB_EVENT_PRESENCE_CHANGED. The initial snapshot can be empty.
static inline PBSysResult pb_presence(PBContext* context, PBParticipant* data, uint32_t capacity,
    uint32_t stride, uint32_t* count) {
    return pb_syscall_invoke(context, 0xa, (uint64_t)(uintptr_t)data, capacity, stride,
        (uint64_t)(uintptr_t)count, 0, 0);
}


// Opened File capabilities belong to one guest generation. Persistent entry
// references below carry no opened-file lifetime or authority of their own.
enum {
    PB_FILE_READ = 1,
    PB_FILE_DIRECTORY = 2,
    PB_FILE_WRITE = 4,
    PB_FILE_EXCLUSIVE = 8,
    PB_FILE_CLOSE_SYNC = 1,
    PB_FILE_CLOSE_DISCARD = 2,
    PB_FILE_KIND_FILE = 1,
    PB_FILE_KIND_DIRECTORY = 2,
    PB_FILE_KIND_SYMLINK = 3,
    PB_FILE_KIND_OTHER = 4,
    PB_FILE_SIZE_VALID = 1,
    PB_FILE_MODIFICATION_TIME_VALID = 2,
    PB_FILE_CONTENTS_IMMUTABLE = 4
};
typedef struct PBFileStatus {
    uint32_t size;
    uint32_t written;
    uint32_t kind;
    uint32_t flags;
    uint64_t file_size;
    int64_t modification_time;
} PBFileStatus;

typedef struct PBFileReference {
    uint32_t size;
    uint32_t written;
    uint64_t volume;
    uint8_t entry_id[15];
    uint8_t reserved;
} PBFileReference;
enum {
    PB_FILE_NOT_PUBLISHED = 0,
    PB_FILE_PUBLISHED = 1,
    PB_FILE_PUBLICATION_UNKNOWN = 2,
    PB_FILE_PUBLISHED_DURABILITY_UNKNOWN = 3,
    PB_FILE_LIST_END = 0,
    PB_FILE_LIST_ENTRY = 1
};
typedef struct PBFileCloseResult {
    uint32_t size;
    uint32_t written;
    int32_t error;
    uint32_t publication;
    uint64_t recovery;
} PBFileCloseResult;
typedef struct PBFileListEntry {
    uint32_t size;
    uint32_t written;
    uint32_t outcome;
    uint32_t kind;
    uint8_t* name_data;
    uint32_t name_capacity;
    uint32_t name_count;
} PBFileListEntry;
// Append copies 1–128 blobs, at most 32 KiB each and 64 KiB including 59-byte
// entry headers and a four-byte count. Empty individual blobs are allowed.
// Blob records use caller size and stride; initialize written to zero.
// Unknown input tails must be zero; stride padding has no meaning.
// Read uses a replica-local opaque cursor; length zero starts at the beginning.
// Append take returns UTF-8 cursor bytes without a terminator. Read take returns
// a little-endian page: cursor_length:u32, cursor bytes, more:u8, count:u32,
// repeated { log[15], id[16], clock:u64, nonce[16], length:u32, payload bytes }.
// Short output publishes count and retains the completion. Never replay an
// append to recover a lost receipt. Resolve logs with pb_entry_resolve_begin.
enum { PB_BLOBLOG_MAX_BYTES = 65536, PB_BLOBLOG_MAX_BLOBS = 128,
       PB_BLOBLOG_MAX_BLOB_BYTES = 32768, PB_BLOBLOG_MAX_CURSOR_BYTES = 104,
       PB_BLOBLOG_MAX_PAGE_BYTES = 65645 };
typedef struct PBBloblogBlob {
    uint32_t size;
    uint32_t written;
    const uint8_t* data;
    uint32_t length;
} PBBloblogBlob;
static inline PBSysResult pb_bloblog_append_begin(PBContext* context, PBVolumeHandle volume,
    const uint8_t log[15], const PBBloblogBlob* blobs, uint32_t count, uint32_t stride,
    const uint8_t append_id[16]) {
    return pb_syscall_invoke(context, 0x170, volume, (uint64_t)(uintptr_t)log,
        (uint64_t)(uintptr_t)blobs, count, stride, (uint64_t)(uintptr_t)append_id);
}
static inline PBSysResult pb_bloblog_append_take(PBContext* context, PBOperationHandle handle,
    uint8_t* data, uint32_t capacity, uint32_t* count) {
    return pb_syscall_invoke(context, 0x171, handle, (uint64_t)(uintptr_t)data, capacity,
        (uint64_t)(uintptr_t)count, 0, 0);
}
static inline PBSysResult pb_bloblog_read_begin(PBContext* context, PBVolumeHandle volume,
    const uint8_t log[15], const uint8_t* cursor, uint32_t length) {
    return pb_syscall_invoke(context, 0x172, volume, (uint64_t)(uintptr_t)log,
        (uint64_t)(uintptr_t)cursor, length, 0, 0);
}
static inline PBSysResult pb_bloblog_read_take(PBContext* context, PBOperationHandle handle,
    uint8_t* data, uint32_t capacity, uint32_t* count) {
    return pb_syscall_invoke(context, 0x173, handle, (uint64_t)(uintptr_t)data, capacity,
        (uint64_t)(uintptr_t)count, 0, 0);
}
// Ordinary named KV uses stable entry identity and current entry permissions.
// Keys are binary and contain 1–511 bytes; a supplied put/chunk is at most 60 KiB.
// Read/keys take write a little-endian frame into a caller byte span. On short output,
// count reports the required frame length and the same completion stays retained.
// Page: present:u8, level:u8, digest[32], key_count:u32, cursor_len:u16,
//       repeated { key_len:u16, value_len:u64, key[key_len] }, cursor[cursor_len].
// Chunk: level:u8, digest[32], total:u64, offset:u64, remaining payload bytes.
// A later page/chunk supplies the returned root; changed backing content conflicts.
enum { PB_KV_MAX_KEY_BYTES = 511, PB_KV_MAX_KEYS = 64, PB_KV_MAX_VALUE_BYTES = 61440,
       PB_KV_MAX_PAGE_BYTES = 33895, PB_KV_MAX_CHUNK_BYTES = 61489 };
typedef struct PBKvQuery {
    uint32_t size;
    uint32_t written;
    uint32_t root_level;
    uint8_t root_digest[32];
    uint32_t has_root;
    uint32_t limit;
    uint32_t reserved;
} PBKvQuery;
typedef struct PBKvRead {
    uint32_t size;
    uint32_t written;
    uint64_t offset;
    uint32_t limit;
    uint32_t has_root;
    uint32_t root_level;
    uint8_t root_digest[32];
    uint32_t reserved;
} PBKvRead;
static inline PBSysResult pb_kv_keys_begin(PBContext* context, const PBEntryHandle* entry, const PBKvQuery* query, const uint8_t* after, uint32_t length) {
    return pb_syscall_invoke(context, 0x140, (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)query, (uint64_t)(uintptr_t)after, length, 0, 0);
}
static inline PBSysResult pb_kv_keys_take(PBContext* context, PBOperationHandle handle,
    uint8_t* data, uint32_t capacity, uint32_t* count) {
    return pb_syscall_invoke(context, 0x141, handle, (uint64_t)(uintptr_t)data, capacity,
        (uint64_t)(uintptr_t)count, 0, 0);
}
static inline PBSysResult pb_kv_read_begin(PBContext* context, const PBEntryHandle* entry, const uint8_t* key, uint32_t length, const PBKvRead* query) {
    return pb_syscall_invoke(context, 0x142, (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)key, length, (uint64_t)(uintptr_t)query, 0, 0);
}
static inline PBSysResult pb_kv_read_take(PBContext* context, PBOperationHandle handle,
    uint8_t* data, uint32_t capacity, uint32_t* count) {
    return pb_syscall_invoke(context, 0x143, handle, (uint64_t)(uintptr_t)data, capacity,
        (uint64_t)(uintptr_t)count, 0, 0);
}
static inline PBSysResult pb_kv_put_begin(PBContext* context, const PBEntryHandle* entry, const uint8_t* key, uint32_t length, const uint8_t* value, uint32_t value_length) {
    return pb_syscall_invoke(context, 0x144, (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)key, length, (uint64_t)(uintptr_t)value, value_length, 0);
}
static inline PBSysResult pb_kv_put_take(PBContext* context, PBOperationHandle handle) {
    return pb_syscall_invoke(context, 0x145, handle, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_kv_delete_begin(PBContext* context, const PBEntryHandle* entry, const uint8_t* key, uint32_t length) {
    return pb_syscall_invoke(context, 0x146, (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)key, length, 0, 0, 0);
}
static inline PBSysResult pb_kv_delete_take(PBContext* context, PBOperationHandle handle) {
    return pb_syscall_invoke(context, 0x147, handle, 0, 0, 0, 0, 0);
}
// Begin owns input and never waits for storage or network. DataReady names the
// exact operation. WouldBlock/BufferTooSmall retain it; short output changes only
// count. Write stages private bytes; only explicit normal Close publishes them.
// Close admission seals the File. A failed publication can return an owned
// read-only recovery File. Disposing an operation never cancels accepted writes
// or an accepted normal Close. File disposal always discards uncommitted data.
static inline PBSysResult pb_file_read_begin(PBContext* context, PBFileHandle file, uint64_t offset, uint32_t limit) {
    return pb_syscall_invoke(context, 0x124, file, offset, limit, 0, 0, 0);
}
static inline PBSysResult pb_file_read_take(PBContext* context, PBOperationHandle handle,
    uint8_t* data, uint32_t capacity, uint32_t* count) {
    return pb_syscall_invoke(context, 0x125, handle, (uint64_t)(uintptr_t)data, capacity,
        (uint64_t)(uintptr_t)count, 0, 0);
}
static inline PBSysResult pb_file_write_begin(PBContext* context, PBFileHandle file, uint64_t offset, const uint8_t* bytes, uint32_t length) {
    return pb_syscall_invoke(context, 0x126, file, offset, (uint64_t)(uintptr_t)bytes, length, 0, 0);
}
static inline PBSysResult pb_file_write_take(PBContext* context, PBOperationHandle handle) {
    return pb_syscall_invoke(context, 0x127, handle, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_open_begin(PBContext* context, PBFileHandle parent, const uint8_t* path, uint32_t length, uint32_t mode) {
    return pb_syscall_invoke(context, 0x150, parent, (uint64_t)(uintptr_t)path, length, mode, 0, 0);
}

static inline PBSysResult pb_file_open_take(PBContext* context, PBOperationHandle operation) {
    return pb_syscall_invoke(context, 0x151, operation, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_status_begin(PBContext* context, PBFileHandle file) {
    return pb_syscall_invoke(context, 0x152, file, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_status_take(PBContext* context, PBOperationHandle operation, PBFileStatus* output) {
    return pb_syscall_invoke(context, 0x153, operation, (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_close_begin(PBContext* context, PBFileHandle file, uint32_t flags) {
    return pb_syscall_invoke(context, 0x154, file, flags, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_close_take(PBContext* context, PBOperationHandle operation, PBFileCloseResult* output) {
    return pb_syscall_invoke(context, 0x155, operation, (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_list_open_begin(PBContext* context, PBFileHandle directory, uint32_t flags) {
    return pb_syscall_invoke(context, 0x156, directory, flags, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_list_open_take(PBContext* context, PBOperationHandle operation) {
    return pb_syscall_invoke(context, 0x157, operation, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_list_next_begin(PBContext* context, PBFileListHandle list) {
    return pb_syscall_invoke(context, 0x158, list, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_list_next_take(PBContext* context, PBOperationHandle operation, PBFileListEntry* output) {
    return pb_syscall_invoke(context, 0x159, operation, (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_list_close(PBContext* context, PBFileListHandle list) {
    return pb_syscall_invoke(context, 0x15a, list, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_operation_dispose(PBContext* context, PBOperationHandle operation) {
    return pb_syscall_invoke(context, 0x15b, operation, 0, 0, 0, 0, 0);
}

static inline PBSysResult pb_file_dispose(PBContext* context, PBFileHandle file) {
    return pb_syscall_invoke(context, 0x15c, file, 0, 0, 0, 0, 0);
}

typedef struct PBSelection {
    uint32_t size;
    uint32_t written;
    uint32_t present;
    uint32_t reserved;
    uint64_t volume;
    uint8_t entry_id[15];
    uint8_t entry_reserved;
} PBSelection;
// At most 64 operations pending per instance. Reserved fields are zero.
// All fixed outputs are required, aligned and disjoint from pointed-to inputs.
// Invalid output is rejected before starting work or consuming a result.
// Poll returns PB_WOULD_BLOCK while pending; wait for the matching ready event.
// After output/handle/kind/capability admission, every result except
// PB_WOULD_BLOCK consumes the operation (including delivery-time denial).
// Admission errors leave it pending. Outputs change only on success.
// Ready events carry slot u32 then generation u32 in 8 little-endian bytes.
// Creation copies all initial fields before begin returns and commits them
// atomically with the object. Begin success means admission, not durability.
// Successful selection: present=1 and a currently readable scoped file.
// Canceled selection: present=0 and a zero file. Selection grants no access.
// Resolve owns the name; Take returns one retained entry capability.
// Renaming an entry does not change an already returned entry ID.
static inline PBSysResult pb_entry_resolve_begin(
    PBContext* context, PBVolumeHandle volume, const uint8_t* name,
    uint32_t length) {
    return pb_syscall_invoke(context, 0x106, volume,
        (uint64_t)(uintptr_t)name, length, 0, 0, 0);
}
static inline PBSysResult pb_entry_resolve_take(
    PBContext* context, PBOperationHandle handle, PBEntryHandle* output) {
    return pb_syscall_invoke(context, 0x107, handle,
        (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}
static inline PBSysResult pb_object_create_begin(
    PBContext* context, const PBEntryHandle* entry, const PBId* type_id,
    const PBFieldInput* changes, uint32_t count, uint32_t stride) {
    return pb_syscall_invoke(
        context,
        0x114,
        (uint64_t)(uintptr_t)entry,
        (uint64_t)(uintptr_t)type_id,
        (uint64_t)(uintptr_t)changes,
        count,
        stride,
        0);
}
static inline PBSysResult pb_object_create_poll(
    PBContext* context, PBOperationHandle handle, PBId* output) {
    return pb_syscall_invoke(
        context,
        0x115,
        handle,
        (uint64_t)(uintptr_t)output,
        0,
        0,
        0,
        0);
}
static inline PBSysResult pb_file_copy_begin(
    PBContext*             context,
    const PBFileReference* source,
    const PBFileReference* destination) {
    return pb_syscall_invoke(
        context,
        0x116,
        (uint64_t)(uintptr_t)source,
        (uint64_t)(uintptr_t)destination,
        0,
        0,
        0,
        0);
}
static inline PBSysResult pb_file_copy_poll(
    PBContext* context, PBOperationHandle handle, uint8_t (*output)[15]) {
    return pb_syscall_invoke(
        context,
        0x117,
        handle,
        (uint64_t)(uintptr_t)output,
        0,
        0,
        0,
        0);
}
static inline PBSysResult pb_file_select_begin(PBContext* context) {
    return pb_syscall_invoke(context, 0x122, 0, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_file_select_poll(
    PBContext* context, PBOperationHandle handle, PBSelection* output) {
    return pb_syscall_invoke(
        context,
        0x123,
        handle,
        (uint64_t)(uintptr_t)output,
        0,
        0,
        0,
        0);
}

typedef struct PBFieldMetadata {
    uint32_t size;
    uint32_t written;
    uint8_t volume_id[16];
    uint8_t object_id[16];
    uint8_t type_id[16];
    uint8_t field_id[16];
    uint8_t revision[32];
    uint8_t principal_id[16];
    uint8_t device_id[16];
    uint32_t state;
    uint32_t provenance;
    uint32_t principal_kind;
    uint32_t reserved;
    uint8_t entry_id[15];
    uint8_t entry_reserved;
} PBFieldMetadata;
typedef struct PBAuthenticatedField {
    uint32_t size;
    uint32_t written;
    PBFieldMetadata* metadata;
    PBValue* values_data;
    uint32_t values_capacity;
    uint32_t values_count;
    uint32_t values_stride;
    uint8_t* bytes_data;
    uint32_t bytes_capacity;
    uint32_t bytes_count;
} PBAuthenticatedField;
// State: 0=absent (metadata payload zero), 1=tombstone, 2=values present (may be empty).
// Provenance: 0=unproven, 1=application edit, 2=rekey writer, not original author.
// Principal kind: 0=none, 1=human, 2=service. Unproven has zero principal/device.
// Revision is opaque and must not be interpreted as a timestamp or sequence.
// Begin owns the IDs and starts one authorized atomic snapshot off the guest turn.
// Take reads retained memory. PB_WOULD_BLOCK and PB_BUFFER_TOO_SMALL retain the
// operation; short output changes counts only. A fresh read needs a new Begin.
// Capacities: at most 4096 values/1 MiB bytes. Descriptor and arrays are disjoint.
// Other errors change nothing. Absent/tombstone success returns zero counts.
static inline PBSysResult pb_object_field_read_authenticated_begin(
    PBContext* context, const PBEntryHandle* entry, const PBId* object_id,
    const PBId* field_id) {
    return pb_syscall_invoke(context, 0x136, (uint64_t)(uintptr_t)entry,
        (uint64_t)(uintptr_t)object_id, (uint64_t)(uintptr_t)field_id,
        0, 0, 0);
}
static inline PBSysResult pb_object_field_read_authenticated_take(
    PBContext* context, PBOperationHandle handle, PBAuthenticatedField* output) {
    return pb_syscall_invoke(context, 0x137, handle,
        (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}

// Inputs remain readable, stable and properly aligned for the call. Outputs
// must be writable, aligned, and disjoint from the context and other arguments.
// A NULL byte pointer is allowed only with zero length/capacity. Functions never
// retain caller buffers. Byte strings are UTF-8, with no NUL terminator required.
// Each turn admits at most 256 log entries and 64 KiB of UTF-8 log bytes.
// Empty strings consume an entry; excess returns PB_LIMIT_EXCEEDED.
static inline PBSysResult pb_log(PBContext* context, const uint8_t* text, uint32_t length) {
    return pb_syscall_invoke(
        context, 0x2, (uint64_t)(uintptr_t)text, (uint64_t)length, 0, 0, 0, 0);
}
// Scene bytes are the UI data format, independent of the operation ABI.
static inline PBSysResult pb_view_submit(PBContext* context, const uint8_t* scene, uint32_t length) {
    return pb_syscall_invoke(
        context, 0x3, (uint64_t)(uintptr_t)scene, (uint64_t)length, 0, 0, 0, 0);
}
// Returns payload byte count or a negative status. A count greater than capacity
// is a size probe: payload and event queue remain unchanged; event->written is zero. PB_WOULD_BLOCK
// means no event. Capacity is at most 265220 bytes. event must be non-NULL even on a size probe. A
// successful call fills complete supported event fields and consumes precisely one event.
static inline PBSysResult pb_event_poll(
    PBContext* context, PBEvent* event, uint8_t* data, uint32_t capacity) {
    return pb_syscall_invoke(
        context,
        0x4,
        (uint64_t)(uintptr_t)event,
        (uint64_t)(uintptr_t)data,
        (uint64_t)capacity,
        0,
        0,
        0);
}
// Wait for one event using the same outputs and size probe as pb_event_poll.
// deadline_ns is in pb_clock_read's monotonic nanoseconds. Zero polls immediately.
// UINT64_MAX waits indefinitely. A queued event takes precedence, including at
// the deadline. An empty queue at expiry returns PB_WOULD_BLOCK.
static inline PBSysResult pb_event_poll_deadline(
    PBContext* context, PBEvent* event, uint8_t* data, uint32_t capacity, uint64_t deadline_ns) {
    return pb_syscall_invoke(
        context,
        0x15,
        (uint64_t)(uintptr_t)event,
        (uint64_t)(uintptr_t)data,
        (uint64_t)capacity,
        deadline_ns,
        0,
        0);
}
static inline PBSysResult pb_identity(PBContext* context, PBIdentity* identity) {
    return pb_syscall_invoke(context, 0x5, (uint64_t)(uintptr_t)identity, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_wake_after(PBContext* context, uint32_t delay_ms) {
    return pb_syscall_invoke(context, 0xb, (uint64_t)delay_ms, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_update_defer(PBContext* context, uint64_t generation) {
    return pb_syscall_invoke(context, 0x9, (uint64_t)generation, 0, 0, 0, 0, 0);
}
// Begin copies every input before returning and yields a generation-scoped
// operation. PB_EVENT_DATA_READY carries that handle, target zero. Take does
// bounded memory work only. PB_WOULD_BLOCK and PB_BUFFER_TOO_SMALL retain the
// operation; a valid terminal take consumes it, including access denial.
static inline PBSysResult pb_object_read_begin(
    PBContext* context, const PBEntryHandle* entry, const PBId* object,
    const PBId* fields, uint32_t count) {
    return pb_syscall_invoke(context, 0x130,
        (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)object,
        (uint64_t)(uintptr_t)fields, count, 0, 0);
}
// Begins own the nested input before returning. Take never re-executes I/O;
// short schema output buffers retain the same completion and update counts.
static inline PBSysResult pb_schema_read_begin(
    PBContext* context, const PBEntryHandle* entry) {
    return pb_syscall_invoke(context, 0x102,
        (uint64_t)(uintptr_t)entry, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_schema_read_take(
    PBContext* context, PBOperationHandle handle, PBSchema* output) {
    return pb_syscall_invoke(context, 0x103,
        handle, (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}
static inline PBSysResult pb_schema_establish_begin(
    PBContext* context, const PBEntryHandle* entry, const PBSchemaTypeInput* types,
    uint32_t count, uint32_t stride) {
    return pb_syscall_invoke(context, 0x104,
        (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)types,
        count, stride, 0, 0);
}
static inline PBSysResult pb_schema_establish_take(PBContext* context, PBOperationHandle handle) {
    return pb_syscall_invoke(context, 0x105,
        handle, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_object_read_take(
    PBContext* context, PBOperationHandle handle, PBObject* output) {
    return pb_syscall_invoke(context, 0x131,
        handle, (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}
static inline PBSysResult pb_object_edit_begin(
    PBContext* context, const PBEntryHandle* entry, const PBId* object,
    const PBFieldInput* fields, uint32_t count, uint32_t stride) {
    return pb_syscall_invoke(context, 0x132,
        (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)object,
        (uint64_t)(uintptr_t)fields, count, stride, 0);
}
static inline PBSysResult pb_object_edit_take(PBContext* context, PBOperationHandle handle) {
    return pb_syscall_invoke(context, 0x133,
        handle, 0, 0, 0, 0, 0);
}
static inline PBSysResult pb_object_query_begin(
    PBContext* context, const PBEntryHandle* entry, const PBQuery* query) {
    return pb_syscall_invoke(context, 0x134,
        (uint64_t)(uintptr_t)entry, (uint64_t)(uintptr_t)query,
        0, 0, 0, 0);
}
static inline PBSysResult pb_object_query_take(
    PBContext* context, PBOperationHandle handle, PBPage* output) {
    return pb_syscall_invoke(context, 0x135,
        handle, (uint64_t)(uintptr_t)output, 0, 0, 0, 0);
}

// Return the current session's positive app-generation token, or a negative PBStatus.
// Starts or reuses live membership without waiting for the first roster.
// Use pb_identity for the 32-byte session ID.
static inline PBSysResult pb_session(PBContext* context) {
    return pb_syscall_invoke(context,19,0,0,0,0,0,0);
}
// Read the current session locally. Outside a session the result is PB_NOT_FOUND.
// Initialize the output with PB_RECORD_INIT(PBSessionInfo).
static inline PBSysResult pb_session_info(PBContext* context, PBSessionInfo* output) {
    return pb_syscall_invoke(context, 0x163, (uint64_t)(uintptr_t)output, 0, 0, 0, 0, 0);
}
// Open an ephemeral named channel. Persistent data belongs in the root volume.
// Channel handles share the operation handle's generation/slot layout.
static inline PBSysResult pb_channel_open(PBContext* context,const uint8_t* name,uint32_t length) {
    return pb_syscall_invoke(context,0x160,(uintptr_t)name,length,0,0,0,0);
}
// action: publish=1, subscribe=2, unsubscribe=3, close=4. Completion uses ChannelTake.
static inline PBSysResult pb_channel_begin(PBContext* context,PBOperationHandle channel,uint32_t action,const uint8_t* bytes,uint32_t length) {
    return pb_syscall_invoke(context,0x161,channel,action,(uintptr_t)bytes,length,0,0);
}
static inline PBSysResult pb_channel_take(PBContext* context,PBOperationHandle operation) {
    return pb_syscall_invoke(context,0x162,operation,0,0,0,0,0);
}

#ifdef __cplusplus
}
#endif
#endif
