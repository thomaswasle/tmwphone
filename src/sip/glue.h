#pragma once

/* Simplified event codes passed to the Rust callback. */
#define SOFIA_EV_REGISTER_OK    1
#define SOFIA_EV_REGISTER_FAIL  2
#define SOFIA_EV_INCOMING_CALL  3
#define SOFIA_EV_CALL_CONNECTED 4
#define SOFIA_EV_CALL_ENDED     5
#define SOFIA_EV_CALL_FAILED    6
/* aux = "local_rtp_port,remote_ip,remote_rtp_port"  e.g. "12345,10.1.2.3,20000" */
#define SOFIA_EV_CALL_MEDIA     7

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
   Must be called from the GTK main thread. */
SofiaCtx *sofia_ctx_create(const char *server, int port,
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

/* Initiate outgoing call.  number may be a bare extension or sip: URI. */
void sofia_call(SofiaCtx *ctx, const char *number);

/* Answer an incoming call (200 OK). */
void sofia_answer(SofiaCtx *ctx);

/* Hang up current call (BYE) or reject incoming call (603). */
void sofia_hangup(SofiaCtx *ctx);

/* Send a DTMF digit over the current call via SIP INFO (application/dtmf-relay).
   digit must be '0'-'9', '*', or '#'. */
void sofia_send_dtmf(SofiaCtx *ctx, char digit);
