// ── Police Department Manual ──────────────────────────────────
// A single static embed: the officer guide & procedures reference posted via
// /police manual setup. One long description rather than fields, so it reads
// as one continuous sheet instead of a stack of separate boxes.
const { EmbedBuilder } = require("discord.js");

const POLICE_MANUAL_COLOR = 0xf59e0b; // orange left bar
function buildPoliceManualEmbed() {
  const divider = "-".repeat(42);
  const section = (title, body) => `${divider}\n**${title}**\n${divider}\n\n${body}`;
  const description = [
    "**DEPARTMENT 📖**\n*__Officer Guide & Procedures__*",
    section("OFFICER CONDUCT 👮",
      "**General Expectations**\n" +
      "• Remain respectful towards civilians, suspects, and fellow officers.\n" +
      "• Do not abuse police equipment, powers, or authority.\n" +
      "• Avoid escalating situations without reason.\n" +
      "• Use common sense in all situations.\n" +
      "• Follow instructions from higher-ranking officers.\n\n" +
      "**Professionalism**\n" +
      "• Speak clearly and respectfully.\n" +
      "• Avoid unnecessary arguments with civilians."),
    section("USE OF FORCE ⚖️",
      "**Force Progression**\n" +
      "Verbal Commands → Non-Lethal Force → Deadly Force\n\n" +
      "**Deadly Force Authorization**\n" +
      "Deadly force may only be used when:\n" +
      "• A suspect presents an immediate threat.\n" +
      "• A suspect is actively using deadly force.\n" +
      "• No reasonable alternative exists."),
    section("TRAFFIC STOPS 🚗",
      "**Initiating a Stop**\n" +
      "• Observe a violation.\n" +
      "• Activate emergency lights.\n" +
      "• Follow until safely stopped.\n\n" +
      "**Conducting a Stop**\n" +
      "• Approach carefully.\n" +
      "• Inform driver of reason.\n" +
      "• Allow explanation.\n" +
      "• Determine warning, citation, or arrest.\n\n" +
      "**Officer Safety**\n" +
      "• Remain aware of passengers.\n" +
      "• Watch for suspicious movements.\n" +
      "• Request backup when necessary."),
    section("VEHICLE PURSUITS 🚔",
      "**When to Pursue**\n" +
      "• Driver refuses to stop.\n" +
      "• Fleeing from serious crime.\n" +
      "• Ongoing threat to public safety.\n\n" +
      "**During a Pursuit**\n" +
      "• Update units continuously.\n" +
      "• Maintain visual contact.\n" +
      "• Avoid unnecessary risks.\n\n" +
      "**Ending a Pursuit**\n" +
      "• Suspect apprehended.\n" +
      "• Suspect incapacitated.\n" +
      "• Suspect lost.\n" +
      "• Danger outweighs necessity."),
    section("FELONY STOPS 🔫",
      "Used for:\n" +
      "• Armed suspects\n" +
      "• Violent offenders\n" +
      "• High-risk vehicles\n\n" +
      "**Procedure**\n" +
      "• Maintain distance.\n" +
      "• Give clear commands.\n" +
      "• Remove occupants one at a time.\n" +
      "• Secure suspects.\n" +
      "• Clear vehicle once detained."),
    section("HOSTAGE SITUATIONS 🏠",
      "**Priorities**\n" +
      "Hostage Safety → Officer Safety → Suspect Apprehension\n\n" +
      "**Procedure**\n" +
      "• Establish perimeter.\n" +
      "• Keep unnecessary personnel away.\n" +
      "• Attempt communication.\n" +
      "• Gather information first.\n\n" +
      "**Use of Force**\n" +
      "Deadly force may be used if the suspect presents an immediate threat to a hostage."),
    section("ACTIVE SHOOTER RESPONSE 🚨",
      "**Response Priorities**\n" +
      "• Locate the shooter.\n" +
      "• Stop the threat.\n" +
      "• Protect civilians.\n" +
      "• Coordinate with responding officers.\n\n" +
      "**Officer Actions**\n" +
      "• Move toward the threat when safe.\n" +
      "• Relay descriptions and locations.\n" +
      "• Work together with units."),
    section("ARREST PROCEDURES 🔗",
      "**Making an Arrest**\n" +
      "• Inform suspect they are under arrest.\n" +
      "• Secure suspect.\n" +
      "• State charges.\n" +
      "• Transport safely.\n\n" +
      "**Searches**\n" +
      "• Arrested suspects\n" +
      "• Vehicles connected to investigations\n" +
      "• Areas where evidence may be located"),
    section("FINAL NOTES 📋",
      "This guide covers the core procedures every officer is expected to know. " +
      "It does not replace training, briefings, or direct orders from a superior, " +
      "and when in doubt, ask before acting. Conduct yourself professionally at all times, " +
      "and remember that civilian safety comes first in every situation."),
  ].join("\n\n");

  return new EmbedBuilder().setColor(POLICE_MANUAL_COLOR).setDescription(description);
}

module.exports = { buildPoliceManualEmbed };
