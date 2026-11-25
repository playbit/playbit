include module type of Playbit_generated

exception Host_error of int32

val log : string -> unit
val submit : string -> unit

(** Fill at most [max_random_bytes] (64 KiB) with cryptographically secure
    platform randomness. Discard the buffer on [Host_error]. *)
val random_fill : bytes -> unit
val host_platform : unit -> int32
val host_limit : int32 -> int32
val quit_on_last_window : bool -> unit

(** Replace the pending wake with a [Wake] event after at least this many
    milliseconds. The host accepts delays of 1,000 to 86,400,000. *)
val wake_after : int -> unit

(** Raw monotonic nanoseconds. All 64 bits are timestamp data. *)
val monotonic_ns : unit -> int64

type event = { generation : int64; kind : int32; target : int32; object_id : int32; data : bytes }

val optional_event_kind_bit : int32
val poll : unit -> event option

(** Wait for one event until the absolute deadline from [monotonic_ns], returning
    [None] on timeout. [0L] polls immediately and [Int64.minus_one] waits
    indefinitely. A queued event wins even at an expired deadline.
    Include [event.poll.deadline] in the package's [required_host_features]. *)
val poll_deadline : int64 -> event option
val run : (unit -> unit) -> unit

module Session : sig
  type channel
  type operation
  type completion = Pending | Ready
  type info = private { internal_id : string; key_secret : string option }

  (** Read the current session's 32-byte public ID and optional 16-byte invitation
      seed. Returns [None] outside a session. Requires the [session.info] feature. *)
  val info : unit -> info option

  (** The 22-character invitation code to append to the deployment's [/s#] URL.
      Anyone with the code can use the invitation. *)
  val code : info -> string option

  type participant = private { identity : string; display_name : string option }

  (** Read the local identity and optional UTF-8 name. Refresh on
      [EventKind.identity_changed], an [int32] advisory event kind. *)
  val local_participant : unit -> participant

  (** Start or reuse live membership in the current session without waiting for
      the first roster. A roster-only app can call this once at startup. *)
  val join : unit -> unit

  (** Read cached members and their optional names. The first read after [join]
      may be empty. Refresh on every [EventKind.presence_changed]. *)
  val participants : unit -> participant list

  (** The local context's account, device and instance IDs, each 16 bytes. *)
  val identity : unit -> string

  (** Cached live members after [join], as tuples in the same layout as [identity]. The local
      member's transport device may differ from the context device. *)
  val presence : unit -> string list

  (** Compare the account and instance IDs of two identity or presence
      tuples. Use the full tuple from [presence] as the live sender identity. *)
  val same_instance : string -> string -> bool

  (** Open or return the live handle for this name in the current instance. *)
  val open_channel : string -> channel
  val subscribe : channel -> operation
  val publish : channel -> string -> operation

  (** Stop the subscription. Messages already queued can still arrive.
      The handle remains available to publish or subscribe again. *)
  val unsubscribe : channel -> operation

  (** Retire the handle once the operation is admitted and stop receiving.
      Take the returned operation to observe cleanup completion. Opening
      the name again returns a fresh handle. *)
  val close : channel -> operation
  val take : operation -> completion

  (** Decode any [DataReady] handle. Match it against an outstanding
      channel operation before calling [take]. *)
  val ready : event -> operation option

  (** Channel, authenticated sender tuple and message bytes. *)
  val message : event -> (channel * string * string) option
  val status : event -> int32 option
end

module Input : sig
  val pointers : int
  val keyboard : int
  val scrolling : int
  val gestures : int
  val gamepads : int

  type pointer = { x : int; y : int; phase : int; buttons : int }
  type key = { key : int; pressed : bool }

  type source = { timestamp_ns : int64; client_id : int32; device_id : int32; modifiers : int }
  type pen = {
    pressure : float; tangential_pressure : float; tilt_x : float; tilt_y : float;
    twist : float; width : float; height : float; altitude : float; azimuth : float;
  }
  type window_pointer = {
    source : source; id : int32; phase : int; kind : int; button : int; click_count : int;
    flags : int; buttons : int; x : float; y : float; dx : float; dy : float;
    canvas_x : float; canvas_y : float; pen : pen option;
  }
  type window_keyboard = {
    source : source; key : int; device_key : int; pressed : bool; repeat : bool; text : Uchar.t array;
  }
  type scroll = {
    source : source; kind : int; phase : int; flags : int;
    x : float; y : float; dx : float; dy : float; wheel_z : float;
  }
  type gesture = {
    source : source; kind : int; phase : int; x : float; y : float; dx : float; dy : float;
    scale : float; rotation : float;
  }
  type gamepad = { source : source; phase : int; control : int; value : float; x : float; y : float }

  type surface = {
    width : int;
    height : int;
    pixel_scale : float;
    inputs : int;
  }

  val pointer : bytes -> pointer
  val key : bytes -> key
  val window_pointer : bytes -> window_pointer
  val window_keyboard : bytes -> window_keyboard
  val scroll : bytes -> scroll
  val gesture : bytes -> gesture
  val gamepad : bytes -> gamepad
  val frame : bytes -> float
  (** Dimensions are in logical units in the current canvas extent. The window
      extent includes the space beneath a full-size titlebar. *)
  val surface : bytes -> surface
end

module View : sig
  val create : unit -> Buffer.t
  val column_start : Buffer.t -> unit
  val column_end : Buffer.t -> unit
  val row_start : Buffer.t -> unit
  val row_end : Buffer.t -> unit

  (** Set semantic colors before writing the root. Each [ThemeColor] role may
      occur once. Colors are Display P3 [0xRRGGBBAA]. *)
  val theme : Buffer.t -> (int * int32) list -> unit

  (** Subscribe to raw window input families before writing the root. Requires [ui.input.v1]. *)
  val window_input : Buffer.t -> int -> unit

  (** Set the complete native window style before writing the root. Combine
      [WindowStyle] flags with [lor]. Requires [ui.window.style.v1]. *)
  val window_style : Buffer.t -> int -> unit

  (** Declare limits on the window's content size in logical units. Call once
      before the root in each scene that needs them. Bounds support fractional
      values up to 2^24. A zero or omitted bound leaves that edge unrestricted.
      [aspect_ratio] is width divided by height. Zero or omission unlocks
      resizing. Requires [ui.window.size-constraints.v1]. *)
  val window_size_constraints :
    ?min_width:float -> ?min_height:float -> ?max_width:float -> ?max_height:float ->
    ?aspect_ratio:float -> Buffer.t -> unit

  val text : Buffer.t -> int -> string -> unit
  val button : Buffer.t -> int32 -> int -> string -> unit

  (** Wrap a single view written by the callback in a native context menu.
      Action IDs are nonzero and unique within the view. Choosing an item
      delivers a [Click] event with its action ID as the target. *)
  val context_menu : Buffer.t -> (int32 * string) list -> (unit -> unit) -> unit

  type rect = { x : int; y : int; width : int; height : int; color : int32 }

  type item =
    | Rect of rect
    | Text of rect * int * bool * string
    | Pixels of rect * int * int * bytes

  val canvas :
    Buffer.t ->
    int32 ->
    width:int ->
    height:int ->
    continuous:bool ->
    item list ->
    unit

  (** Draw a transparent root canvas in the padded content viewport.
      A context menu may wrap the canvas. Requires [ui.canvas.viewport.v1]. *)
  val viewport_canvas :
    Buffer.t ->
    int32 ->
    width:int ->
    height:int ->
    continuous:bool ->
    item list ->
    unit

  type canvas_extent = Inline | Viewport | Window

  (** [Viewport] and [Window] are root extents. [Window] removes the host
      content insets and extends behind the controls of a decorated window.
      Its logical dimensions may reach [max_window_canvas_edge] per axis.
      Inline canvases, padded viewports and source bitmaps keep [max_canvas_edge].
      [clear_color] fills the canvas before its items, in Display P3 [0xRRGGBBAA].
      Declare [ui.canvas.clear.v1] for a clear color, [ui.canvas.viewport.v1]
      for [Viewport], and [ui.canvas.window.v1] for [Window]. *)
  val canvas_with_options :
    ?clear_color:int32 ->
    extent:canvas_extent ->
    Buffer.t ->
    int32 ->
    width:int ->
    height:int ->
    continuous:bool ->
    item list ->
    unit

  val finish : Buffer.t -> string
end

module Data : sig
  type volume
  type volume_info = { volume : volume; can_write : bool }
  type volume_role = Root | Session | User | User_session | Local | Local_session
  type entry
  type operation
  type 'a completion = Pending | Ready of 'a

  val volume : volume_role -> volume
  val root_volume : unit -> volume

  (** The selected handle and write permission from the same enumeration. *)
  val volume_info : volume_role -> volume_info
  val resolve : volume -> string -> operation
  val take_entry : operation -> entry completion
  type key_cursor

  (** List binary KV keys in ascending order. Pass the returned cursor to
      continue the same snapshot. If the snapshot changes, [take_keys] raises
      [Host_error (Int32.of_int Error.conflict)]. *)
  val keys : ?cursor:key_cursor -> entry -> operation
  val take_keys : operation -> (string list * key_cursor option) completion
  val read : entry -> string -> limit:int -> operation
  val take_read : operation -> limit:int -> bytes completion
  val put : entry -> string -> bytes -> operation
  val take_put : operation -> unit completion
  val dispose : operation -> unit
end

module Audio : sig
  type t
  type operation
  type 'a completion = Pending | Ready of 'a

  val open_begin : preferred_sample_rate:int -> operation
  val open_take : operation -> t completion
  val dispose : operation -> unit
  val close : t -> unit
  val sample_rate : t -> int
  val volume : t -> float
  val set_volume : t -> float -> unit
end

module Sound : sig
  type t
  type instance

  val loop : int32
  val open_pcm : Audio.t -> channel_count:int -> sample_rate:int -> bytes -> t
  val close : t -> unit
  val instance : t -> int32 -> instance
  val play : instance -> gain:float -> start:int64 -> length:int64 -> flags:int32 -> unit
  val tell : instance -> int64
  val seek : instance -> int64 -> unit
  val set_gain : instance -> float -> unit
  val set_balance : instance -> float -> unit
  val stop : instance -> unit
end
