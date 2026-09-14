// PM2 process definition for the messaging bridge (WhatsApp + Telegram).
//
// Secrets are read from the environment, never written here: this file is in
// git, so anything literal in it is in the history too. Set them in the shell
// that starts PM2, or in an env file PM2 loads.
//
// Required:
//   CLAUDE_API_KEY       — the bridge runs its own Claude agent
//   API0_INTERNAL_SECRET — must match the store's
//
// Optional:
//   META_APP_SECRET      — WhatsApp webhook signature validation. Unset means
//                          validation is skipped, which is unsafe in production.
//                          Telegram does not use it; it authenticates each
//                          update with the per-bot secret stored at register
//                          time, so Telegram is safe without this.
module.exports = {
    apps: [{
        name: "api0-bridge",
        script: "./target/release/whatsapp-bridge",
        instances: 1,
        exec_mode: "fork",
        env: {
            NODE_ENV: "production",
            // Production ports differ from the dev config.yaml — see the
            // comments in config_production.yaml before changing this.
            CONFIG_PATH: "config_production.yaml",
            LOG_PATH_API0: "/var/log/api0.log",
            CLAUDE_API_KEY: process.env.CLAUDE_API_KEY,
            API0_INTERNAL_SECRET: process.env.API0_INTERNAL_SECRET,
            META_APP_SECRET: process.env.META_APP_SECRET,
            RUST_LOG: "debug",
            RUST_BACKTRACE: "1"
        },
        error_file: "./logs/bridge-error.log",
        out_file: "./logs/bridge-out.log",
        log_file: "./logs/bridge-combined.log",
        time: true,
        max_memory_restart: "500M"
    }]
};
