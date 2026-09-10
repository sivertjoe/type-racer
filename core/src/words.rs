/// A bank of common, short English words to build random typing-race
/// sentences from - the same approach typing-test sites use. Swap this out
/// (or add more banks and pick between them) if you want different content
/// later; nothing outside this file needs to change.
const WORDS: &[&str] = &[
    "the", "of", "and", "a", "to", "in", "is", "you", "that", "it", "he", "was", "for", "on", "are", "as", "with",
    "his", "they", "at", "be", "this", "have", "from", "or", "one", "had", "by", "word", "but", "not", "what", "all",
    "were", "we", "when", "your", "can", "said", "there", "use", "each", "which", "she", "do", "how", "their", "if",
    "will", "up", "other", "about", "out", "many", "then", "them", "these", "so", "some", "her", "would", "make",
    "like", "him", "into", "time", "has", "look", "two", "more", "write", "go", "see", "number", "no", "way", "could",
    "people", "my", "than", "first", "water", "been", "call", "who", "its", "now", "find", "long", "down", "day",
    "did", "get", "come", "made", "may", "part", "over", "new", "sound", "take", "only", "little", "work", "know",
    "place", "year", "live", "me", "back", "give", "most", "very", "after", "thing", "our", "just", "name", "good",
    "sentence", "man", "think", "say", "great", "where", "help", "through", "much", "before", "line", "right", "too",
    "mean", "old", "any", "same", "tell", "boy", "follow", "came", "want", "show", "also", "around", "form", "three",
    "small", "set", "put", "end", "why", "again", "turn", "here", "off", "went", "old", "number", "great", "tell",
    "men", "say", "small", "every", "found", "still", "between", "mane", "should", "home", "big", "high", "such",
    "follow", "act", "why", "ask", "men", "change", "went", "light", "kind", "off", "need", "house", "picture", "try",
    "us", "again", "animal", "point", "mother", "world", "near", "build", "self", "earth", "father", "head", "stand",
    "own", "page", "should", "country", "found", "answer", "school", "grow", "study", "still", "learn", "plant",
    "cover", "food", "sun", "four", "between", "state", "keep", "eye", "never", "last", "let", "thought", "city",
    "tree", "cross", "farm", "hard", "start", "might", "story", "saw", "far", "sea", "draw", "left", "late", "run",
    "while", "press", "close", "night", "real", "life", "few", "north", "open", "seem", "together", "next", "white",
    "children", "begin", "got", "walk", "example", "ease", "paper", "group", "always", "music", "those", "both",
    "mark", "often", "letter", "until", "mile", "river", "car", "feet", "care", "second", "book", "carry", "took",
    "science", "eat", "room", "friend", "began", "idea", "fish", "mountain", "stop", "once", "base", "hear", "horse",
    "cut", "sure", "watch", "color", "face", "wood", "main", "enough", "plain", "girl", "usual", "young", "ready",
    "above", "ever", "red", "list", "though", "feel", "talk", "bird", "soon", "body", "dog", "family", "direct",
    "leave", "song", "measure", "door", "product", "black", "short", "numeral", "class", "wind", "question", "happen",
    "complete", "ship", "area", "half", "rock", "order", "fire", "south", "problem", "piece", "told", "knew", "pass",
    "since", "top", "whole", "king", "space", "heard", "best", "hour", "better", "true", "during", "hundred", "five",
    "remember", "step", "early", "hold", "west", "ground", "interest", "reach", "fast", "verb", "sing", "listen",
    "six", "table", "travel", "less", "morning", "ten", "simple", "several", "vowel", "toward", "war", "lay",
    "against", "pattern", "slow", "center", "love", "person", "money", "serve", "appear", "road", "map", "rain",
    "rule", "govern", "pull", "cold", "notice", "voice", "unit", "power", "town", "fine", "certain", "fly", "fall",
    "lead", "cry", "dark", "machine", "note", "wait", "plan", "figure", "star", "box", "noun", "field", "rest",
    "correct", "able", "pound", "done", "beauty", "drive", "stood", "contain", "front", "teach", "week", "final",
    "gave", "green", "oh", "quick", "develop", "ocean", "warm", "free", "minute", "strong", "special", "mind",
    "behind", "clear", "tail", "produce", "fact", "street", "inch", "multiply", "nothing", "course", "stay", "wheel",
    "full", "force", "blue", "object", "decide", "surface", "deep", "moon", "island", "foot", "system", "busy",
    "test", "record", "boat", "common", "gold", "possible", "plane", "stead", "dry", "wonder", "laugh", "thousand",
    "ago", "ran", "check", "game", "shape", "equate", "hot", "miss", "brought", "heat", "snow", "tire", "bring",
    "yes", "distant", "fill", "east", "paint", "language", "among",
];

/// Words for one race: `count` random picks from [`WORDS`], joined with
/// spaces. Words can repeat - a small trade for keeping this dependency-
/// free and each word individually easy to type.
pub fn random_sentence(count: usize) -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..count).map(|_| WORDS[rng.random_range(0..WORDS.len())]).collect::<Vec<_>>().join(" ")
}

const WORDS_PER_RACE: usize = 15;

/// A fresh random sentence for one race.
pub fn generate_sentence() -> String {
    random_sentence(WORDS_PER_RACE)
}
