//! Police Department Manual.
//!
//! A single static embed: the officer guide & procedures reference posted via
//! `/police manual setup`. One long description rather than fields, so it
//! reads as one continuous sheet instead of a stack of separate boxes.

use serenity::builder::CreateEmbed;

const POLICE_MANUAL_COLOR: u32 = 0xf59e0b; // orange left bar

fn section(title: &str, body: &str) -> String {
    let divider = "-".repeat(42);
    format!("{divider}\n**{title}**\n{divider}\n\n{body}")
}

pub fn build_police_manual_embed() -> CreateEmbed {
    CreateEmbed::new().color(POLICE_MANUAL_COLOR).description(build_police_manual_embed_text())
}

/// The manual body on its own, so it can be length-checked in tests.
pub fn build_police_manual_embed_text() -> String {
    let parts = vec![
        "**DEPARTMENT 📖**\n*__Officer Guide & Procedures__*".to_string(),
        section(
            "OFFICER CONDUCT 👮",
            "**General Expectations**\n\
             • Remain respectful towards civilians, suspects, and fellow officers.\n\
             • Do not abuse police equipment, powers, or authority.\n\
             • Avoid escalating situations without reason.\n\
             • Use common sense in all situations.\n\
             • Follow instructions from higher-ranking officers.\n\n\
             **Professionalism**\n\
             • Speak clearly and respectfully.\n\
             • Avoid unnecessary arguments with civilians.",
        ),
        section(
            "USE OF FORCE ⚖️",
            "**Force Progression**\n\
             Verbal Commands → Non-Lethal Force → Deadly Force\n\n\
             **Deadly Force Authorization**\n\
             Deadly force may only be used when:\n\
             • A suspect presents an immediate threat.\n\
             • A suspect is actively using deadly force.\n\
             • No reasonable alternative exists.",
        ),
        section(
            "TRAFFIC STOPS 🚗",
            "**Initiating a Stop**\n\
             • Observe a violation.\n\
             • Activate emergency lights.\n\
             • Follow until safely stopped.\n\n\
             **Conducting a Stop**\n\
             • Approach carefully.\n\
             • Inform driver of reason.\n\
             • Allow explanation.\n\
             • Determine warning, citation, or arrest.\n\n\
             **Officer Safety**\n\
             • Remain aware of passengers.\n\
             • Watch for suspicious movements.\n\
             • Request backup when necessary.",
        ),
        section(
            "VEHICLE PURSUITS 🚔",
            "**When to Pursue**\n\
             • Driver refuses to stop.\n\
             • Fleeing from serious crime.\n\
             • Ongoing threat to public safety.\n\n\
             **During a Pursuit**\n\
             • Update units continuously.\n\
             • Maintain visual contact.\n\
             • Avoid unnecessary risks.\n\n\
             **Ending a Pursuit**\n\
             • Suspect apprehended.\n\
             • Suspect incapacitated.\n\
             • Suspect lost.\n\
             • Danger outweighs necessity.",
        ),
        section(
            "FELONY STOPS 🔫",
            "Used for:\n\
             • Armed suspects\n\
             • Violent offenders\n\
             • High-risk vehicles\n\n\
             **Procedure**\n\
             • Maintain distance.\n\
             • Give clear commands.\n\
             • Remove occupants one at a time.\n\
             • Secure suspects.\n\
             • Clear vehicle once detained.",
        ),
        section(
            "HOSTAGE SITUATIONS 🏠",
            "**Priorities**\n\
             Hostage Safety → Officer Safety → Suspect Apprehension\n\n\
             **Procedure**\n\
             • Establish perimeter.\n\
             • Keep unnecessary personnel away.\n\
             • Attempt communication.\n\
             • Gather information first.\n\n\
             **Use of Force**\n\
             Deadly force may be used if the suspect presents an immediate threat to a hostage.",
        ),
        section(
            "ACTIVE SHOOTER RESPONSE 🚨",
            "**Response Priorities**\n\
             • Locate the shooter.\n\
             • Stop the threat.\n\
             • Protect civilians.\n\
             • Coordinate with responding officers.\n\n\
             **Officer Actions**\n\
             • Move toward the threat when safe.\n\
             • Relay descriptions and locations.\n\
             • Work together with units.",
        ),
        section(
            "ARREST PROCEDURES 🔗",
            "**Making an Arrest**\n\
             • Inform suspect they are under arrest.\n\
             • Secure suspect.\n\
             • State charges.\n\
             • Transport safely.\n\n\
             **Searches**\n\
             • Arrested suspects\n\
             • Vehicles connected to investigations\n\
             • Areas where evidence may be located",
        ),
        section(
            "FINAL NOTES 📋",
            "This guide covers the core procedures every officer is expected to know. \
             It does not replace training, briefings, or direct orders from a superior, \
             and when in doubt, ask before acting. Conduct yourself professionally at all times, \
             and remember that civilian safety comes first in every situation.",
        ),
    ];

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    /// Discord counts an embed description in UTF-16 code units (the same unit
    /// JS `.length` reports), which is what the 4096 limit applies to. The JS
    /// implementation produced exactly 3667 of them.
    #[test]
    fn manual_matches_js_length_and_fits_discord_limit() {
        let text = super::build_police_manual_embed_text();
        let utf16_len = text.encode_utf16().count();
        assert_eq!(utf16_len, 3667, "police manual text drifted from the JS original");
        assert!(utf16_len <= 4096, "police manual would exceed Discord's description limit");
    }
}
