//! Wholesome word list for generating room names.
//!
//! Room names follow the pattern: adjective-noun-noun
//! Words are curated to be positive, family-friendly, and easy to say aloud.

use rand::prelude::IndexedRandom;

/// Wholesome adjectives for room names.
const ADJECTIVES: &[&str] = &[
    "amber",
    "azure",
    "blazing",
    "breezy",
    "bright",
    "brilliant",
    "calm",
    "cheerful",
    "clear",
    "clever",
    "cozy",
    "crisp",
    "crystal",
    "dancing",
    "dazzling",
    "dewy",
    "dreamy",
    "eager",
    "emerald",
    "enchanted",
    "evening",
    "fabled",
    "fair",
    "faithful",
    "fancy",
    "festive",
    "fluffy",
    "flying",
    "folksy",
    "forest",
    "friendly",
    "frosty",
    "gentle",
    "gilded",
    "gleaming",
    "glowing",
    "golden",
    "graceful",
    "grand",
    "groovy",
    "happy",
    "harmonic",
    "hazy",
    "hearty",
    "hidden",
    "honey",
    "humming",
    "icy",
    "ivy",
    "jade",
    "jazzy",
    "jolly",
    "joyful",
    "jubilant",
    "keen",
    "kind",
    "lacy",
    "leafy",
    "light",
    "lively",
    "lovely",
    "lucky",
    "lunar",
    "lush",
    "magic",
    "majestic",
    "mellow",
    "melodic",
    "merry",
    "mighty",
    "misty",
    "moonlit",
    "morning",
    "mossy",
    "musical",
    "mystic",
    "nimble",
    "noble",
    "northern",
    "oaken",
    "ocean",
    "painted",
    "pastel",
    "peaceful",
    "pearly",
    "peppy",
    "perky",
    "playful",
    "plucky",
    "polished",
    "pretty",
    "prism",
    "proud",
    "pure",
    "purple",
    "quiet",
    "radiant",
    "rainy",
    "rapid",
    "restful",
    "rhythmic",
    "rising",
    "rosy",
    "ruby",
    "rustic",
    "saffron",
    "sage",
    "sandy",
    "satin",
    "scarlet",
    "serene",
    "shady",
    "shimmering",
    "shiny",
    "silent",
    "silky",
    "silver",
    "singing",
    "sleepy",
    "smooth",
    "snowy",
    "soft",
    "solar",
    "soothing",
    "southern",
    "sparkling",
    "spicy",
    "spirited",
    "spring",
    "starlit",
    "starry",
    "steady",
    "stormy",
    "summer",
    "sunny",
    "sunset",
    "swift",
    "tender",
    "tidal",
    "tranquil",
    "twilight",
    "velvet",
    "verdant",
    "vintage",
    "violet",
    "vivid",
    "wandering",
    "warm",
    "waving",
    "western",
    "wild",
    "willow",
    "windy",
    "winter",
    "wise",
    "wistful",
    "wonder",
    "wooden",
    "zesty",
    "zippy",
];

/// Wholesome nouns for room names.
const NOUNS: &[&str] = &[
    "acorn",
    "anchor",
    "anthem",
    "apple",
    "arrow",
    "aurora",
    "autumn",
    "bamboo",
    "beacon",
    "beach",
    "bear",
    "bell",
    "berry",
    "birch",
    "bird",
    "blossom",
    "boat",
    "book",
    "boulder",
    "branch",
    "breeze",
    "bridge",
    "brook",
    "butterfly",
    "cabin",
    "candle",
    "canyon",
    "castle",
    "cedar",
    "chime",
    "chorus",
    "cider",
    "circle",
    "citrus",
    "cliff",
    "clover",
    "coast",
    "comet",
    "coral",
    "cottage",
    "crane",
    "creek",
    "crescent",
    "crystal",
    "daisy",
    "dance",
    "dawn",
    "deer",
    "delta",
    "desert",
    "dove",
    "dream",
    "drum",
    "dune",
    "eagle",
    "echo",
    "elm",
    "ember",
    "fable",
    "falcon",
    "fern",
    "field",
    "finch",
    "fire",
    "fjord",
    "flame",
    "flower",
    "flute",
    "fog",
    "forest",
    "fountain",
    "fox",
    "frost",
    "garden",
    "gem",
    "glade",
    "glacier",
    "glen",
    "grove",
    "guitar",
    "harbor",
    "harp",
    "harvest",
    "haven",
    "hawk",
    "hazel",
    "heath",
    "heron",
    "hill",
    "hollow",
    "honey",
    "horizon",
    "horn",
    "hymn",
    "iris",
    "island",
    "ivy",
    "jasmine",
    "jay",
    "journey",
    "juniper",
    "kayak",
    "kettle",
    "kite",
    "lake",
    "lantern",
    "lark",
    "laurel",
    "leaf",
    "legend",
    "light",
    "lily",
    "linden",
    "lion",
    "lotus",
    "lullaby",
    "luna",
    "lyre",
    "maple",
    "marsh",
    "meadow",
    "melody",
    "mesa",
    "mist",
    "moon",
    "moss",
    "mountain",
    "music",
    "nest",
    "night",
    "north",
    "oak",
    "oasis",
    "ocean",
    "olive",
    "orbit",
    "orchid",
    "oriole",
    "osprey",
    "owl",
    "palm",
    "pansy",
    "paper",
    "park",
    "path",
    "peak",
    "pebble",
    "pepper",
    "petal",
    "piano",
    "pier",
    "pine",
    "plum",
    "poem",
    "pond",
    "poplar",
    "prairie",
    "prism",
    "quail",
    "quartz",
    "quest",
    "rain",
    "rainbow",
    "raven",
    "reef",
    "rhythm",
    "ridge",
    "ripple",
    "river",
    "robin",
    "rock",
    "rose",
    "sage",
    "sail",
    "sand",
    "sea",
    "seed",
    "shade",
    "shell",
    "shore",
    "silk",
    "silver",
    "sky",
    "snow",
    "song",
    "sonnet",
    "south",
    "sparrow",
    "spire",
    "spring",
    "spruce",
    "star",
    "stone",
    "storm",
    "story",
    "stream",
    "summer",
    "summit",
    "sun",
    "sunrise",
    "sunset",
    "surf",
    "swan",
    "tale",
    "thistle",
    "thunder",
    "tide",
    "timber",
    "trail",
    "tree",
    "tulip",
    "tune",
    "valley",
    "velvet",
    "vine",
    "viola",
    "violet",
    "voyage",
    "waltz",
    "water",
    "wave",
    "west",
    "whale",
    "wheat",
    "willow",
    "wind",
    "wing",
    "winter",
    "wisteria",
    "wolf",
    "wonder",
    "wood",
    "wren",
    "yarn",
    "zephyr",
];

/// Generate a random room name in adjective-noun-noun format.
pub fn generate_room_name() -> String {
    let mut rng = rand::rng();

    let adj = ADJECTIVES.choose(&mut rng).unwrap_or(&"sunny");
    let noun1 = NOUNS.choose(&mut rng).unwrap_or(&"garden");
    let noun2 = NOUNS.choose(&mut rng).unwrap_or(&"melody");

    format!("{}-{}-{}", adj, noun1, noun2)
}

/// Validate a room name format (should be word-word-word).
pub fn is_valid_room_name(name: &str) -> bool {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    // Check each part is non-empty and lowercase alphanumeric
    parts
        .iter()
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase()))
}

/// Parse a room input which can be:
/// - `room-name` (just room)
/// - `room-name@peerid` (room with peer)
/// - `http://host/#room-name` (full URL)
/// - `http://host/#room-name@peerid` (full URL with peer)
///
/// Returns Some((room_with_optional_peer, is_valid)) or None if completely unparseable.
pub fn parse_room_input(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    // Try to extract from URL hash
    let room_part = if input.contains('#') {
        // Extract everything after the last #
        input.rsplit('#').next()?
    } else if input.starts_with("http://") || input.starts_with("https://") {
        // URL without hash - not valid
        return None;
    } else {
        // Raw room input
        input
    };

    // Now room_part is either "room-name" or "room-name@peerid"
    // Validate the room name part (before @)
    let room_name = room_part.split('@').next().unwrap_or(room_part);

    if is_valid_room_name(room_name) {
        Some(room_part.to_string())
    } else {
        None
    }
}

/// Check if a room input is valid (can be URL or room@peer format).
pub fn is_valid_room_input(input: &str) -> bool {
    parse_room_input(input).is_some()
}

/// Generate a QR code SVG string for a room URL.
pub fn generate_room_qr_svg(room_name: &str, base_url: &str) -> String {
    use qrcode::{QrCode, render::svg};

    let url = format!("{}#{}", base_url, room_name);

    match QrCode::new(url.as_bytes()) {
        Ok(code) => {
            code.render()
                .min_dimensions(200, 200)
                .dark_color(svg::Color("#f1f5f9")) // Light text color
                .light_color(svg::Color("#0f172a")) // Dark background
                .build()
        }
        Err(_) => {
            // Return a placeholder if QR generation fails
            "<svg width=\"200\" height=\"200\"><text x=\"50%\" y=\"50%\" text-anchor=\"middle\">QR Error</text></svg>".to_string()
        }
    }
}

/// Hash a room name to a 32-byte topic ID for gossip.
///
/// BLAKE3's derive-key mode fixes the algorithm and domain separation so topic
/// identity is stable across Rust and platform releases.
pub fn room_name_to_topic_id(name: &str) -> [u8; 32] {
    blake3::derive_key(
        "xyz.wondering.walkie-songie room topic v1",
        name.to_ascii_lowercase().as_bytes(),
    )
}

/// Room-v4 topic identity. Delegates to the one shared core derivation used by
/// the transport layer.
pub fn room_name_to_topic_id_v4(name: &str) -> [u8; 32] {
    crate::room::v4::room_topic_v4(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_room_name_format() {
        let name = generate_room_name();
        let parts: Vec<&str> = name.split('-').collect();
        assert_eq!(parts.len(), 3, "Room name should have 3 parts: {}", name);
        assert!(
            parts.iter().all(|p| !p.is_empty()),
            "No empty parts: {}",
            name
        );
    }

    #[test]
    fn test_generate_room_name_unique() {
        // Generate several names and check they're not all the same
        let names: Vec<String> = (0..10).map(|_| generate_room_name()).collect();
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert!(unique.len() > 1, "Should generate different names");
    }

    #[test]
    fn test_is_valid_room_name() {
        assert!(is_valid_room_name("sunny-garden-melody"));
        assert!(is_valid_room_name("happy-river-song"));
        assert!(is_valid_room_name("abc-def-ghi"));

        assert!(!is_valid_room_name("sunny-garden")); // Only 2 parts
        assert!(!is_valid_room_name("sunny")); // Only 1 part
        assert!(!is_valid_room_name("sunny-garden-melody-extra")); // 4 parts
        assert!(!is_valid_room_name("Sunny-garden-melody")); // Uppercase
        assert!(!is_valid_room_name("sunny--melody")); // Empty middle
        assert!(!is_valid_room_name("")); // Empty
    }

    #[test]
    fn test_word_lists_not_empty() {
        assert!(!ADJECTIVES.is_empty());
        assert!(!NOUNS.is_empty());
        assert!(
            ADJECTIVES.len() >= 100,
            "Should have at least 100 adjectives"
        );
        assert!(NOUNS.len() >= 150, "Should have at least 150 nouns");
    }

    #[test]
    fn test_room_name_to_topic_id_deterministic() {
        let id1 = room_name_to_topic_id("sunny-garden-melody");
        let id2 = room_name_to_topic_id("sunny-garden-melody");
        assert_eq!(id1, id2, "Same name should produce same topic ID");
    }

    #[test]
    fn test_room_name_to_topic_id_case_insensitive() {
        let id1 = room_name_to_topic_id("sunny-garden-melody");
        let id2 = room_name_to_topic_id("SUNNY-GARDEN-MELODY");
        let id3 = room_name_to_topic_id("Sunny-Garden-Melody");
        assert_eq!(id1, id2, "Should be case insensitive");
        assert_eq!(id1, id3, "Should be case insensitive");
    }

    #[test]
    fn test_room_name_to_topic_id_different_names() {
        let id1 = room_name_to_topic_id("sunny-garden-melody");
        let id2 = room_name_to_topic_id("happy-river-song");
        assert_ne!(
            id1, id2,
            "Different names should produce different topic IDs"
        );
    }

    #[test]
    fn room_v4_topic_helper_delegates_to_the_shared_derivation() {
        assert_eq!(
            room_name_to_topic_id_v4("Sunny-Garden-Melody"),
            crate::room::v4::room_topic_v4("sunny-garden-melody"),
        );
    }
}
