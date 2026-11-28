//! Map protocol subtitle metadata onto the output container's stream metadata.

use super::ManifestContainer;
use crate::manifest_types::ManifestSubtitleMetadata;

/// ISO 639-1 to ISO 639-2/T. MP4 stores a three-letter language code in its
/// track header, while HLS and DASH commonly signal BCP 47 tags.
const ISO_639_1_TO_2: &[(&str, &str)] = &[
    ("aa", "aar"),
    ("ab", "abk"),
    ("ae", "ave"),
    ("af", "afr"),
    ("ak", "aka"),
    ("am", "amh"),
    ("an", "arg"),
    ("ar", "ara"),
    ("as", "asm"),
    ("av", "ava"),
    ("ay", "aym"),
    ("az", "aze"),
    ("ba", "bak"),
    ("be", "bel"),
    ("bg", "bul"),
    ("bh", "bih"),
    ("bi", "bis"),
    ("bm", "bam"),
    ("bn", "ben"),
    ("bo", "bod"),
    ("br", "bre"),
    ("bs", "bos"),
    ("ca", "cat"),
    ("ce", "che"),
    ("ch", "cha"),
    ("co", "cos"),
    ("cr", "cre"),
    ("cs", "ces"),
    ("cu", "chu"),
    ("cv", "chv"),
    ("cy", "cym"),
    ("da", "dan"),
    ("de", "deu"),
    ("dv", "div"),
    ("dz", "dzo"),
    ("ee", "ewe"),
    ("el", "ell"),
    ("en", "eng"),
    ("eo", "epo"),
    ("es", "spa"),
    ("et", "est"),
    ("eu", "eus"),
    ("fa", "fas"),
    ("ff", "ful"),
    ("fi", "fin"),
    ("fj", "fij"),
    ("fo", "fao"),
    ("fr", "fra"),
    ("fy", "fry"),
    ("ga", "gle"),
    ("gd", "gla"),
    ("gl", "glg"),
    ("gn", "grn"),
    ("gu", "guj"),
    ("gv", "glv"),
    ("ha", "hau"),
    ("he", "heb"),
    ("hi", "hin"),
    ("ho", "hmo"),
    ("hr", "hrv"),
    ("ht", "hat"),
    ("hu", "hun"),
    ("hy", "hye"),
    ("hz", "her"),
    ("ia", "ina"),
    ("id", "ind"),
    ("ie", "ile"),
    ("ig", "ibo"),
    ("ii", "iii"),
    ("ik", "ipk"),
    ("io", "ido"),
    ("is", "isl"),
    ("it", "ita"),
    ("iu", "iku"),
    ("ja", "jpn"),
    ("jv", "jav"),
    ("ka", "kat"),
    ("kg", "kon"),
    ("ki", "kik"),
    ("kj", "kua"),
    ("kk", "kaz"),
    ("kl", "kal"),
    ("km", "khm"),
    ("kn", "kan"),
    ("ko", "kor"),
    ("kr", "kau"),
    ("ks", "kas"),
    ("ku", "kur"),
    ("kv", "kom"),
    ("kw", "cor"),
    ("ky", "kir"),
    ("la", "lat"),
    ("lb", "ltz"),
    ("lg", "lug"),
    ("li", "lim"),
    ("ln", "lin"),
    ("lo", "lao"),
    ("lt", "lit"),
    ("lu", "lub"),
    ("lv", "lav"),
    ("mg", "mlg"),
    ("mh", "mah"),
    ("mi", "mri"),
    ("mk", "mkd"),
    ("ml", "mal"),
    ("mn", "mon"),
    ("mr", "mar"),
    ("ms", "msa"),
    ("mt", "mlt"),
    ("my", "mya"),
    ("na", "nau"),
    ("nb", "nob"),
    ("nd", "nde"),
    ("ne", "nep"),
    ("ng", "ndo"),
    ("nl", "nld"),
    ("nn", "nno"),
    ("no", "nor"),
    ("nr", "nbl"),
    ("nv", "nav"),
    ("ny", "nya"),
    ("oc", "oci"),
    ("oj", "oji"),
    ("om", "orm"),
    ("or", "ori"),
    ("os", "oss"),
    ("pa", "pan"),
    ("pi", "pli"),
    ("pl", "pol"),
    ("ps", "pus"),
    ("pt", "por"),
    ("qu", "que"),
    ("rm", "roh"),
    ("rn", "run"),
    ("ro", "ron"),
    ("ru", "rus"),
    ("rw", "kin"),
    ("sa", "san"),
    ("sc", "srd"),
    ("sd", "snd"),
    ("se", "sme"),
    ("sg", "sag"),
    ("si", "sin"),
    ("sk", "slk"),
    ("sl", "slv"),
    ("sm", "smo"),
    ("sn", "sna"),
    ("so", "som"),
    ("sq", "sqi"),
    ("sr", "srp"),
    ("ss", "ssw"),
    ("st", "sot"),
    ("su", "sun"),
    ("sv", "swe"),
    ("sw", "swa"),
    ("ta", "tam"),
    ("te", "tel"),
    ("tg", "tgk"),
    ("th", "tha"),
    ("ti", "tir"),
    ("tk", "tuk"),
    ("tl", "tgl"),
    ("tn", "tsn"),
    ("to", "ton"),
    ("tr", "tur"),
    ("ts", "tso"),
    ("tt", "tat"),
    ("tw", "twi"),
    ("ty", "tah"),
    ("ug", "uig"),
    ("uk", "ukr"),
    ("ur", "urd"),
    ("uz", "uzb"),
    ("ve", "ven"),
    ("vi", "vie"),
    ("vo", "vol"),
    ("wa", "wln"),
    ("wo", "wol"),
    ("xh", "xho"),
    ("yi", "yid"),
    ("yo", "yor"),
    ("za", "zha"),
    ("zh", "zho"),
    ("zu", "zul"),
];

fn mp4_language_code(language: &str) -> Option<String> {
    let primary = language.split('-').next()?.trim();
    if primary.len() == 3 && primary.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Some(primary.to_ascii_lowercase());
    }
    if primary.len() != 2 || !primary.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }

    let primary = primary.to_ascii_lowercase();
    ISO_639_1_TO_2
        .iter()
        .find(|(alpha_2, _)| *alpha_2 == primary)
        .map(|(_, alpha_3)| (*alpha_3).to_string())
}

/// Add protocol-derived metadata to the first subtitle stream in the next FFmpeg output.
pub(super) fn append_ffmpeg_metadata(
    command: &mut tokio::process::Command,
    container: ManifestContainer,
    metadata: Option<&ManifestSubtitleMetadata>,
) {
    let Some(metadata) = metadata else {
        return;
    };

    if let Some(name) = metadata.name.as_deref().filter(|name| !name.is_empty()) {
        let key = if container == ManifestContainer::Mp4 {
            // The MP4 muxer writes handler_name to the track's display name.
            "handler_name"
        } else {
            "title"
        };
        command
            .arg("-metadata:s:s:0")
            .arg(format!("{key}={name}"));
    }

    if let Some(language) = metadata.language.as_deref() {
        let language = match container {
            ManifestContainer::Mp4 => mp4_language_code(language),
            ManifestContainer::Mkv => Some(language.to_string()),
            ManifestContainer::Ts => None,
        };
        if let Some(language) = language {
            command
                .arg("-metadata:s:s:0")
                .arg(format!("language={language}"));
        }
    }

    let disposition = match (metadata.is_default, metadata.is_forced) {
        (true, true) => "default+forced",
        (true, false) => "default",
        (false, true) => "forced",
        (false, false) => "0",
    };
    command.arg("-disposition:s:0").arg(disposition);
}

#[cfg(test)]
mod tests {
    use super::mp4_language_code;

    #[test]
    fn maps_bcp47_primary_language_to_the_mp4_iso639_code() {
        assert_eq!(mp4_language_code("en-US").as_deref(), Some("eng"));
        assert_eq!(mp4_language_code("DE-de").as_deref(), Some("deu"));
        assert_eq!(mp4_language_code("zh-Hant-TW").as_deref(), Some("zho"));
        assert_eq!(mp4_language_code("und").as_deref(), Some("und"));
        assert_eq!(mp4_language_code("x-private"), None);
        assert_eq!(mp4_language_code("en_US"), None);
    }
}
