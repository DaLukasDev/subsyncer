use super::*;

const SRT: &str = "1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hello</i> there\r\n\r\n2\r\n00:00:03,000 --> 00:00:04,000 X1:10 X2:20\r\n[door slams]\r\n\r\n3\r\n00:01:00,250 --> 00:01:02,000\r\nLine one\r\nLine two\r\n";

const VTT: &str = "WEBVTT\nKind: captions\n\nNOTE a comment\n\nintro\n00:01.000 --> 00:02.500 align:start\nHello <00:01.500>karaoke\n\n01:00:00.000 --> 01:00:01.000\nLate\n";

const ASS: &str = "\u{feff}[Script Info]\nTitle: x\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,Hello, world\\Nsecond\nComment: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,note\nDialogue: 0,0:00:05.00,0:00:06.00,Signs,,0,0,0,,{\\pos(1,2)}EXIT\n";

#[test]
fn srt_parse() {
    let f = SubtitleFile::parse(SRT.as_bytes(), None).unwrap();
    assert_eq!(f.format(), Format::Srt);
    let c = f.cues();
    assert_eq!(c.len(), 3);
    assert_eq!(c[0].interval(), Interval::new(1000, 2500));
    assert_eq!(c[1].interval(), Interval::new(3000, 4000));
    assert_eq!(c[2].interval(), Interval::new(60_250, 62_000));
    assert_eq!(f.cue_text(2), "Line one\r\nLine two");
    assert!(f.is_dialogue(0));
    assert!(!f.is_dialogue(1));
    let d: Vec<usize> = f.dialogue_cues().iter().map(|x| x.0).collect();
    assert_eq!(d, vec![0, 2]);
}

#[test]
fn vtt_parse() {
    let f = SubtitleFile::parse(VTT.as_bytes(), None).unwrap();
    assert_eq!(f.format(), Format::Vtt);
    let c = f.cues();
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].interval(), Interval::new(1000, 2500));
    assert_eq!(c[0].inline.len(), 1);
    assert_eq!(c[0].inline[0].ms, 1500);
    assert_eq!(c[1].interval(), Interval::new(3_600_000, 3_601_000));
}

#[test]
fn ass_parse() {
    let f = SubtitleFile::parse(ASS.as_bytes(), None).unwrap();
    assert_eq!(f.format(), Format::Ass);
    assert!(f.encoding().bom);
    let c = f.cues();
    assert_eq!(c.len(), 3);
    assert_eq!(c[0].interval(), Interval::new(1000, 2500));
    assert_eq!(f.cue_text(0), "Hello, world\\Nsecond");
    assert!(c[1].comment);
    assert!(f.is_dialogue(0));
    assert!(!f.is_dialogue(1));
    assert!(!f.is_dialogue(2));
}

#[test]
fn unshifted_render_is_byte_identical() {
    for (src, hint) in [(SRT, None), (VTT, None), (ASS, None), (SRT, Some(Format::Srt))] {
        let f = SubtitleFile::parse(src.as_bytes(), hint).unwrap();
        assert_eq!(f.render(|_, _, ms| ms).unwrap(), src.as_bytes());
        assert_eq!(f.original_bytes().unwrap(), src.as_bytes());
    }
}

#[test]
fn shifted_render_changes_only_timestamps() {
    let f = SubtitleFile::parse(SRT.as_bytes(), None).unwrap();
    let out = String::from_utf8(f.render(|_, _, ms| ms + 1500).unwrap()).unwrap();
    let expected = SRT
        .replace("00:00:01,000 --> 00:00:02,500", "00:00:02,500 --> 00:00:04,000")
        .replace("00:00:03,000 --> 00:00:04,000", "00:00:04,500 --> 00:00:05,500")
        .replace("00:01:00,250 --> 00:01:02,000", "00:01:01,750 --> 00:01:03,500");
    assert_eq!(out, expected);

    let f = SubtitleFile::parse(VTT.as_bytes(), None).unwrap();
    let out = String::from_utf8(f.render(|_, _, ms| ms - 500).unwrap()).unwrap();
    assert!(out.contains("00:00.500 --> 00:02.000 align:start\nHello <00:01.000>karaoke"));
    assert!(out.contains("00:59:59.500 --> 01:00:00.500"));

    let f = SubtitleFile::parse(ASS.as_bytes(), None).unwrap();
    let out = String::from_utf8(f.render(|_, _, ms| ms + 10_004).unwrap()).unwrap();
    assert!(out.contains("Dialogue: 0,0:00:11.00,0:00:12.50,Default,,0,0,0,,Hello, world\\Nsecond"));
    assert!(out.contains("Comment: 0,0:00:13.00,0:00:14.00,"));
    assert!(out.starts_with('\u{feff}'));
}

#[test]
fn negative_times_clamp_to_zero() {
    let f = SubtitleFile::parse(SRT.as_bytes(), None).unwrap();
    let out = String::from_utf8(f.render(|_, _, ms| ms - 2000).unwrap()).unwrap();
    assert!(out.contains("00:00:00,000 --> 00:00:00,500"));
}

#[test]
fn lf_only_and_no_trailing_newline() {
    let src = "1\n00:00:01,000 --> 00:00:02,000\nA\n\n2\n00:00:03,000 --> 00:00:04,000\nB";
    let f = SubtitleFile::parse(src.as_bytes(), None).unwrap();
    assert_eq!(f.cues().len(), 2);
    assert_eq!(f.cue_text(1), "B");
    assert_eq!(f.render(|_, _, ms| ms).unwrap(), src.as_bytes());
}

#[test]
fn utf16_srt_roundtrip_and_shift() {
    let mut bytes = vec![0xFF, 0xFE];
    for u in SRT.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    let f = SubtitleFile::parse(&bytes, None).unwrap();
    assert_eq!(f.cues().len(), 3);
    assert_eq!(f.render(|_, _, ms| ms).unwrap(), bytes);
    let shifted = f.render(|_, _, ms| ms + 1000).unwrap();
    let text = encoding_rs::UTF_16LE.decode_with_bom_removal(&shifted).0.into_owned();
    assert!(text.contains("00:00:02,000 --> 00:00:03,500"));
    assert_eq!(&shifted[..2], &[0xFF, 0xFE]);
}

#[test]
fn ssa_v4_format() {
    let src = "[Script Info]\nScriptType: v4.00\n\n[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: Marked=0,0:00:01.00,0:00:02.00,Default,NTP,0000,0000,0000,!Effect,Hi there\n";
    let f = SubtitleFile::parse(src.as_bytes(), None).unwrap();
    assert_eq!(f.format(), Format::Ass);
    assert_eq!(f.cues()[0].interval(), Interval::new(1000, 2000));
    assert_eq!(f.cue_text(0), "Hi there");
}

#[test]
fn garbage_is_rejected() {
    assert!(SubtitleFile::parse(b"hello world\nnothing here", None).is_err());
}
