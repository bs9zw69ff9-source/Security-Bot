pub mod anti_ping;
pub mod applications;
pub mod chain_of_command;
pub mod guild_settings;
pub mod lockdown;
pub mod mod_rates;
pub mod muted_roles;
pub mod tickets;
pub mod warnings;

/// Run every one-time home-guild seed / backfill, in the same order the JS
/// bot ran them at module load. Safe to call on every boot: each is guarded.
pub fn run_migrations() {
    guild_settings::migrate_env_to_home_guild();
    tickets::migrate_tickets_to_home_guild();
    tickets::migrate_ticket_category();
    tickets::migrate_wasteland_tickets();
    applications::migrate_applications_to_home_guild();
    applications::migrate_application_requirements();
    applications::migrate_staff_questions_v2();
    applications::migrate_family_questions_v2();
    applications::migrate_nypd_questions_v2();
    applications::migrate_nypd_questions_v3();
    applications::migrate_nypd_review_channel_v2();
    applications::migrate_unseed_wasteland_from_wrong_guild();
    applications::migrate_wasteland_applications();
    applications::migrate_wasteland_roles_v2();
    applications::migrate_wasteland_staff_application();
    applications::migrate_wasteland_staff_role_v1();
    chain_of_command::migrate_chain_of_command_to_home_guild();
    chain_of_command::migrate_police_chain_of_command_to_home_guild();
}
