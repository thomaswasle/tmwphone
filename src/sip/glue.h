#pragma once

/* Simplified event codes passed to the Rust callback. */
#define SOFIA_EV_REGISTER_OK    1
#define SOFIA_EV_REGISTER_FAIL  2
#define SOFIA_EV_INCOMING_CALL  3
#define SOFIA_EV_CALL_CONNECTED 4
#define SOFIA_EV_CALL_ENDED     5
#define SOFIA_EV_CALL_FAILED    6
/* aux = "local_rtp_port,remote_ip,remote_rtp_port,payload"  e.g. "12345,10.1.2.3,20000,0" */
#define SOFIA_EV_CALL_MEDIA     7

#define SOFIA_EV_TRANSFER_OK       8   /* blind transfer accepted */
#define SOFIA_EV_TRANSFER_FAILED   9   /* blind transfer rejected */
#define SOFIA_EV_CONSULT_RINGING  10   /* consultation call ringing */
#define SOFIA_EV_CONSULT_CONNECTED 11  /* consultation call answered */
#define SOFIA_EV_CONSULT_MEDIA    12   /* aux = "local_port,remote_ip,remote_port,payload" */
#define SOFIA_EV_CONSULT_ENDED    13   /* consultation call ended/cancelled */

typedef void (*sofia_event_cb_t)(
    int         event,      /* SOFIA_EV_* */
    int         status,     /* SIP status code */
    const char *phrase,     /* SIP reason phrase */
    const char *aux,        /* from-URI for INCOMING_CALL, NULL otherwise */
    void       *userdata
);

typedef struct SofiaCtx SofiaCtx;

/* Create context integrated with the current GLib main context.
   server/port are used to pick the correct local interface on multi-homed hosts.
   proxy is an optional outbound proxy host (may be NULL or empty).
   Must be called from the GTK main thread. */
SofiaCtx *sofia_ctx_create(const char *server, int port, const char *proxy,
                            sofia_event_cb_t cb, void *userdata);

/* Destroy context.  No callbacks will fire after this returns. */
void sofia_ctx_destroy(SofiaCtx *ctx);

/* Send REGISTER.  Handles 401/407 digest auth automatically. */
void sofia_register(SofiaCtx   *ctx,
                    const char *server,
                    int         port,
                    const char *user,
                    const char *password,
                    const char *display_name);

/* Send REGISTER with Expires: 0 (unregister). */
void sofia_unregister(SofiaCtx *ctx);

/* Force an immediate registration refresh (re-sends REGISTER on the existing
   handle without tearing down the SIP stack). Safe to call while on a call. */
void sofia_reregister(SofiaCtx *ctx);

/* Initiate outgoing call.  number may be a bare extension or sip: URI. */
void sofia_call(SofiaCtx *ctx, const char *number);

/* Answer an incoming call (200 OK). */
void sofia_answer(SofiaCtx *ctx);

/* Hang up current call (BYE) or reject incoming call (603). */
void sofia_hangup(SofiaCtx *ctx);

/* Put current call on hold (hold=1) or resume it (hold=0) via re-INVITE.
   On hold the SDP direction is set to "sendonly"; on resume to "sendrecv". */
void sofia_set_hold(SofiaCtx *ctx, int hold);

/* Send a DTMF digit over the current call via SIP INFO (application/dtmf-relay).
   digit must be '0'-'9', '*', or '#'. */
void sofia_send_dtmf(SofiaCtx *ctx, char digit);

/* Blind-transfer the current call to number via SIP REFER. */
void sofia_blind_transfer(SofiaCtx *ctx, const char *number);

/* Put current call on hold and dial number as a consultation call. */
void sofia_start_consultation(SofiaCtx *ctx, const char *number);

/* Transfer the held call to the consultation party (REFER), then end consultation. */
void sofia_complete_transfer(SofiaCtx *ctx);

/* Cancel consultation: hang up consult call and resume held primary call. */
void sofia_cancel_consultation(SofiaCtx *ctx);
