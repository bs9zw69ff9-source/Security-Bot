pub mod anti_ping;
pub mod applications;
pub mod backups;
pub mod chain_of_command;
pub mod guild_settings;
pub mod lockdown;
pub mod message_store;
pub mod mod_rates;
pub mod muted_roles;
pub mod tickets;
pub mod tunables;
pub mod warnings;

/// Run every one-time home-guild seed / backfill, in the same order the JS
/// bot ran them at module load. Safe to call on every boot: each is guarded.
pub fn run_migrations() {
    guild_settings::migrate_env_to_home_guild();
    anti_ping::migrate_env_to_home_guild();
    // The GWPD server first: it is set up by its own id, and going first means
    // a GUILD_ID ever pointed at it finds it configured and leaves it alone.
    tickets::migrate_gwpd_tickets();
    tickets::migrate_gwpd_staff_reports_confidential();
    applications::migrate_gwpd_guild();
    applications::migrate_gwpd_panel_v2();
    applications::migrate_gwpd_staff_application();
    applications::migrate_gwpd_review_v3();
    tickets::migrate_tickets_to_home_guild();
    tickets::migrate_ticket_category();
    tickets::migrate_wasteland_tickets();
    applications::migrate_applications_to_home_guild();
    applications::migrate_application_requirements();
    applications::migrate_staff_questions_v2();
    applications::migrate_family_questions_v2();
    applications::migrate_gwpd_questions_v2();
    applications::migrate_gwpd_questions_v3();
    applications::migrate_gwpd_gang_members_wording();
    applications::migrate_gwpd_review_channel_v2();
    applications::migrate_nypd_to_gwpd();
    applications::migrate_unseed_wasteland_from_wrong_guild();
    applications::migrate_wasteland_applications();
    applications::migrate_wasteland_roles_v2();
    applications::migrate_wasteland_staff_application();
    applications::migrate_wasteland_staff_role_v1();
    chain_of_command::migrate_chain_of_command_to_home_guild();
    chain_of_command::migrate_police_chain_of_command_to_home_guild();
}
