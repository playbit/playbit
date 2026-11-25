#include <stdint.h>
#include <stddef.h>
#include <notify.h>
typedef struct IosEngine IosEngine;
typedef struct IosReply IosReply;
typedef struct IosScene IosScene;
typedef struct { const uint8_t *bytes; size_t len; } DatabaseBytes;
IosReply *pb_ios_database(IosEngine *, uint64_t, const uint8_t *, size_t);
DatabaseBytes pb_ios_reply_database(const IosReply *);
// Reply fields are borrowed until pb_ios_reply_free. Copy before releasing.
typedef struct {
    const uint8_t *generation; // 16 borrowed bytes, or null for a non-step reply.
    uint64_t sequence, wake_after_ns;
    uint8_t has_wake;
} IosSchedule;
typedef struct {
    uint64_t request_id;
    uint8_t has_response;
    uint32_t error_code; // 0 success, 1 failure, 2 corrupt store.
    const char *error, *background_error;
} IosCompletion;
IosCompletion pb_ios_reply_completion(const IosReply *);
typedef struct { const uint8_t *bytes; size_t len; } RecoveryText;
typedef struct { uint64_t value; uint8_t present; } RecoveryNumber;
typedef struct {
    uint32_t category, kind;
    RecoveryNumber revision, pending_save, restored_sessions, restored_volumes;
    RecoveryText secret;
} RecoveryFields;
// category: 0 absent, 1 local state, 2 prepared key.
// All text spans are borrowed until pb_ios_reply_free.
RecoveryFields pb_ios_reply_recovery(const IosReply *);
typedef struct {
    const uint8_t *engine; // 16 bytes, null when this is not a log reply.
    uint64_t instance, next, dropped;
    size_t count;
    uint8_t reset;
} LogFields;
typedef struct { const uint8_t *bytes; size_t len; } LogLine;
LogFields pb_ios_reply_logs(const IosReply *);
LogLine pb_ios_reply_log_line(const IosReply *, size_t index);
typedef struct {
    uint8_t present;
    const uint8_t *instance; // 16 borrowed bytes; identifies this local app owner.
    uint64_t generation;
    const uint8_t *build; size_t build_len;
    const uint8_t *invitation; size_t invitation_len; // Null/zero when sharing is unavailable.
    const uint8_t *availability; size_t availability_len;
    uint32_t update_kind; // 0 absent, 1 consent, 2 failed, 3 paused.
    const uint8_t *update_build; size_t update_build_len;
    const uint8_t *update_message; size_t update_message_len;
} PresentationFields;
PresentationFields pb_ios_reply_presentation(const IosReply *);
typedef struct {
    uint32_t kind; // 0 absent; 1 idle, 2 running, 3 complete, 4 paused, 5 canceling, 6 failed.
    const uint8_t *session; size_t session_len;
    const uint8_t *message; size_t message_len;

    uint32_t update_kind; // 0 absent, 1 consent, 2 failed, 3 paused.
    const uint8_t *update_build; size_t update_build_len;
    const uint8_t *update_message; size_t update_message_len;
} TransferFields;
TransferFields pb_ios_reply_transfer(const IosReply *);
typedef struct {
    // 0 absent, 1 personal volumes selected, 2 display name, 3 sign-in available,
    // 4 code sent, 5 ordered, 6 forgotten, 7 quit instance, 8 created session ID,
    // 9 opened instance (generation > 0), 10 prepared session ID.
    uint32_t kind;
    uint8_t flag;
    const uint8_t *text; size_t text_len;
    uint64_t generation;
    const uint8_t *secret;
    size_t secret_len;
} ControlFields;
ControlFields pb_ios_reply_control(const IosReply *);
typedef struct {
    uint32_t category; // 0 absent, 1 preview, 2 status, 3 pause result, 4 copied link.
    uint8_t paused, retained;
    // 0 absent, 1 paused, 2 checking, 3 sign-in, 4 registration, 5 app,
    // 6 database, 7 peer, 8 ready, 9 denied, 10 failed.
    uint32_t state;
    const uint8_t *session; size_t session_len;
    const uint8_t *root_name; size_t root_name_len;
    const uint8_t *build; size_t build_len;
    const uint8_t *message; size_t message_len;
    const uint8_t *invitation; size_t invitation_len;
    const uint8_t *availability; size_t availability_len;
} InvitationFields;
InvitationFields pb_ios_reply_invitation(const IosReply *);
typedef struct {
    const uint8_t *session; size_t session_len;
    size_t volumes, grants;
    uint8_t has_grants;
} PersonalVolumeFields;
typedef struct { const uint8_t *id; const uint8_t *name; size_t name_len; } PersonalVolumeRow;
typedef struct { const uint8_t *id; uint8_t write; } PersonalGrant;
PersonalVolumeFields pb_ios_reply_personal_volumes(const IosReply *);
// IDs borrow 16 bytes; null means the indexed entry is absent.
PersonalVolumeRow pb_ios_reply_personal_volume(const IosReply *, size_t index);
PersonalGrant pb_ios_reply_personal_grant(const IosReply *, size_t index);
typedef struct { const uint8_t *bytes; size_t len; } StatusText;
typedef struct {
    uint8_t present, signed_in, replacement_required;
    RecoveryFields recovery;
    StatusText recovery_error;
} SignInFields;
SignInFields pb_ios_reply_sign_in(const IosReply *);
typedef struct {
    uint32_t kind; // 0 absent, 1 published app page, 2 app display name.
    size_t count;
    StatusText next; // Optional app-ID cursor.
    StatusText build, name;
} DeveloperFields;
typedef struct {
    const uint8_t *publisher; // 16-byte publisher ID.
    StatusText app_id, build;
    uint32_t state; // 1 active, 2 disabled, 3 deleted.
} AppFields;
DeveloperFields pb_ios_reply_developer(const IosReply *);
AppFields pb_ios_reply_app(const IosReply *, size_t index);
typedef struct {
    StatusText account;
    uint8_t enrolled, unlocked, initialized, replacement_required, agreement_required;
    StatusText display_name, email, account_key_backup;
    size_t sessions, volumes, examples, invitations;
} StatusFields;
typedef struct { StatusText id, build; const uint8_t *root; StatusText name, build_kind, build_message, reference; } SessionFields;
typedef struct { StatusText id, name, description, invitation, source; } ExampleFields;
StatusFields pb_ios_reply_status(const IosReply *);
SessionFields pb_ios_status_session(const IosReply *, size_t index);
PersonalVolumeRow pb_ios_status_volume(const IosReply *, size_t index);
ExampleFields pb_ios_status_example(const IosReply *, size_t index);
InvitationFields pb_ios_status_invitation(const IosReply *, size_t index);
InvitationFields pb_ios_status_watch(const IosReply *);
IosSchedule pb_ios_reply_schedule(const IosReply *);
const IosScene *pb_ios_reply_scene(const IosReply *);
void pb_ios_reply_free(IosReply *);
typedef struct {
    uint32_t kind, id, style, state, window_input;
    uint8_t disabled, multiline, read_only;
    const uint8_t *text;
    size_t text_len;
    const uint8_t *placeholder;
    size_t placeholder_len, children;
    uint32_t width, height, frames;
    size_t items;
    uint32_t clear_color;
    uint8_t has_clear_color;
} SceneNode;
typedef struct {
    uint32_t kind, x, y, width, height, color, font_size;
    uint8_t bold;
    const uint8_t *text;
    size_t text_len;
    uint32_t source_width, source_height;
    const uint8_t *pixels;
    size_t pixels_len;
} SceneCanvasItem;
// kind: 1 column, 2 row, 3 text, 4 button, 5 text field, 6 checkbox, 7 canvas.
SceneNode pb_ios_scene_node(const IosScene *);
const IosScene *pb_ios_scene_child(const IosScene *, size_t index);
// Canvas item kind: 0 rectangle, 1 text, 2 sRGB RGBA8 pixels. Spans live until reply_free.
SceneCanvasItem pb_ios_scene_canvas_item(const IosScene *, size_t index);
// channel: 0 public, 1 testing. Unknown values and conflicting profile markers fail.
IosReply *pb_ios_open(const char *profile, uint8_t channel, IosEngine **output);
IosReply *pb_ios_app_input(IosEngine *, uint64_t id, const uint8_t *instance, size_t instance_len, uint32_t kind, uint32_t target, uint32_t object_id, const uint8_t *data, size_t data_len);
IosReply *pb_ios_status(IosEngine *, uint64_t id);
IosReply *pb_ios_prepare_sign_in(IosEngine *, uint64_t id);
IosReply *pb_ios_transfer_status(IosEngine *, uint64_t id);
IosReply *pb_ios_cancel_transfer(IosEngine *, uint64_t id);
IosReply *pb_ios_recovery_status(IosEngine *, uint64_t id);
IosReply *pb_ios_prepare_recovery(IosEngine *, uint64_t id);
IosReply *pb_ios_save_recovery(IosEngine *, uint64_t id);
// Local frame, input, logs and Quit use a canonical base58 session-instance ID (16 decoded bytes).
// Open and prepare use a base58 session ID (at most 44 bytes). Open never prepares or runs a guest turn.
IosReply *pb_ios_prepare_session(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_app_frame(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_open_app(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_quit_app(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_check_updates(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_personal_volumes(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_invitation(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_forget_session(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_watch_invitation(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_resume_invitation(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_pause_invitation(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_start_join(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_preview_invitation(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
StatusText pb_ios_user_agreement(void);
IosReply *pb_ios_accept_agreement(IosEngine *, uint64_t id, const uint8_t *account, size_t len);
IosReply *pb_ios_set_display_name(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_sign_in(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_request_code(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_recover_credentials(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_app_logs(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_list_apps(IosEngine *, uint64_t id, const uint8_t *text, size_t len);
IosReply *pb_ios_create_app_session(IosEngine *, uint64_t id, const uint8_t *publisher, size_t publisher_len, const uint8_t *app_id, size_t app_id_len);
IosReply *pb_ios_get_app_display(IosEngine *, uint64_t id, const uint8_t *publisher, size_t publisher_len, const uint8_t *app_id, size_t app_id_len, const uint8_t *build, size_t build_len);
IosReply *pb_ios_sign_in_with_key(IosEngine *, uint64_t id, const uint8_t *first, size_t first_len, const uint8_t *second, size_t second_len);
// sessions contains count raw 32-byte IDs; count <= 256.
IosReply *pb_ios_order_sessions(IosEngine *, uint64_t id, const uint8_t *sessions, size_t count);
// volumes contains count raw 16-byte IDs; writes contains count 0/1 flags.
// count <= 128; version is base58 text for 32 decoded bytes (at most 44 UTF-8 bytes). All spans are borrowed only for the call.
IosReply *pb_ios_select_personal_volumes(IosEngine *, uint64_t id, const uint8_t *session, size_t session_len, const uint8_t *volumes, const uint8_t *writes, size_t count);
// Returns null on success or an owned error freed with pb_ios_string_free.
// Canonical base58 instance identity. These affect local playback only.
int32_t pb_ios_audio_active(const uint8_t *text, size_t len, uint8_t active);
void pb_ios_audio_foreground(uint8_t active);
char *pb_ios_suspend(IosEngine *);
IosReply *pb_ios_resume(IosEngine *);
IosReply *pb_ios_wake(IosEngine *);
IosReply *pb_ios_delivery_ready(IosEngine *);
// Success transfers a descriptor through output; caller closes it after source cancellation.
// Failure writes -1 and returns an owned error freed with pb_ios_string_free.
char *pb_ios_completion_descriptor(IosEngine *, int32_t *output);
IosReply *pb_ios_complete_background(IosEngine *);
void pb_ios_close(IosEngine *);
char *pb_ios_stop_for_sign_out(IosEngine *);
char *pb_ios_keep_profile(const char *profile);
char *pb_ios_stop_for_replacement(IosEngine *);
char *pb_ios_resolve_replacement(const char *profile, uint8_t replace);
void pb_ios_string_free(char *);
// Background artwork worker only; output is exactly 120 * 120 * 4 bytes, premultiplied RGBA.
// Returns null on success (present=0 or 1); on failure present=0 and an owned UTF-8
// error is returned. Free errors with pb_ios_string_free.
// published=0 reads a cached package; published=1 reads its bounded AP display projection.
char *pb_ios_icon_read(const char *profile, const char *build, uint8_t published, uint8_t *output, size_t capacity, uint8_t *present);
// Software intervals only, on the recorder's local monotonic clock. Zero means
// uninitialized/disabled. The Engine worker owns recorder initialization.
typedef enum {
    PB_IOS_TRACE_SESSIONS_LAYOUT = 1,
    PB_IOS_TRACE_SESSIONS_LABELS = 2,
    PB_IOS_TRACE_CONSENT_CONSTRUCTION = 3,
    PB_IOS_TRACE_CONSENT_SNAPSHOT = 4,
    PB_IOS_TRACE_CONSENT_CELL = 5,
    PB_IOS_TRACE_CONSENT_CHOICE = 6,
    PB_IOS_TRACE_CONSENT_RESPONSE = 7,
    PB_IOS_TRACE_WORKER_DELIVERY = 8,
    PB_IOS_TRACE_ARTWORK_APPLY = 9,
    PB_IOS_TRACE_ARTWORK_READY = 10,
    PB_IOS_TRACE_NEW_SESSION_LAYOUT = 11,
    PB_IOS_TRACE_NEW_SESSION_CONTROLS = 12
} IosTraceStage;
uint64_t pb_ios_trace_start(void);
void pb_ios_trace_end(IosTraceStage, uint64_t source_ns);
#ifdef PHASE1_QUALIFICATION
IosReply *pb_ios_qualification_seed(IosEngine *, const char *bundle, uint8_t personal_volumes, uint8_t account_profile, uint8_t session_list);
#endif

#ifdef PHASE1_QUALIFICATION
void pb_ios_qualification_panic(void);
uint8_t pb_ios_qualification_malformed_scene(void);
#endif

uint8_t pb_ios_saved_account_available(void);
IosReply *pb_ios_saved_account(IosEngine *, uint64_t);
IosReply *pb_ios_retry_account_key_backup(IosEngine *, uint64_t);

char *pb_ios_reset_profile(const char *profile);

// Protected File Provider account snapshot; never log the returned snapshot.
char *pb_ios_provider_state(IosEngine *, const uint8_t *, size_t, char **);
char *pb_ios_provider_state_committed(IosEngine *);
typedef struct ProviderPushRegistration ProviderPushRegistration;
char *pb_ios_provider_push_prepare(IosEngine *, const char *, const uint8_t *, size_t, uint8_t, const char *, uint8_t, ProviderPushRegistration **);
char *pb_ios_provider_push_run(ProviderPushRegistration *);
uint64_t pb_ios_provider_push_generation(IosEngine *);
