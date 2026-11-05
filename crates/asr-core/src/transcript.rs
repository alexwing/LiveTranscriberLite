//! Historial de la transcripcion y volcado a disco.

use std::fmt::Write as _;
use std::path::Path;

use asr_audio::Source;

/// Una linea cerrada. Se cierra cuando el motor termina un segmento.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub source: Source,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

/// Acumula las lineas cerradas y el texto parcial en curso de cada fuente.
///
/// Los tiempos vienen del motor (audio de cada flujo), no de aqui. Mic y
/// sistema arrancan a la vez y se alimentan sin huecos, asi que son comparables
/// y las lineas de las dos fuentes se ordenan juntas por `start_ms`.
#[derive(Debug, Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    /// Texto parcial por fuente, solo para pintarlo en vivo.
    partial: [Option<String>; 2],
}

fn slot(source: Source) -> usize {
    match source {
        Source::System => 0,
        Source::Mic => 1,
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lineas cerradas, en el orden en que llegaron.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.partial = [None, None];
    }

    /// Texto parcial de una fuente, para pintarlo en vivo.
    pub fn pending(&self, source: Source) -> Option<&str> {
        self.partial[slot(source)].as_deref()
    }

    /// El parcial trae el texto completo del segmento: sustituye, no concatena.
    pub fn set_partial(&mut self, source: Source, text: &str) {
        self.partial[slot(source)] = Some(text.to_string());
    }

    /// Cierra el segmento de una fuente. Devuelve la linea si tenia contenido.
    pub fn push_final(
        &mut self,
        source: Source,
        text: &str,
        start_ms: u64,
        end_ms: u64,
    ) -> Option<Entry> {
        self.partial[slot(source)] = None;
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let entry = Entry {
            source,
            start_ms,
            end_ms: end_ms.max(start_ms),
            text: text.to_string(),
        };
        self.entries.push(entry.clone());
        Some(entry)
    }

    /// Las lineas por `start_ms`, y a igualdad en el orden en que llegaron.
    fn sorted(&self) -> Vec<&Entry> {
        let mut sorted: Vec<&Entry> = self.entries.iter().collect();
        sorted.sort_by_key(|e| e.start_ms);
        sorted
    }

    /// Una linea por entrada: `[hh:mm:ss] [sistema|micro] texto`.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for entry in self.sorted() {
            let _ = writeln!(
                out,
                "[{}] [{}] {}",
                clock(entry.start_ms),
                label(entry.source),
                entry.text
            );
        }
        out
    }

    /// SRT numerado. El prefijo con la fuente solo aparece si hay entradas de
    /// las dos: con una sola no aporta nada.
    pub fn to_srt(&self) -> String {
        let both = self.entries.iter().any(|e| e.source == Source::System)
            && self.entries.iter().any(|e| e.source == Source::Mic);
        let mut out = String::new();
        for (i, entry) in self.sorted().into_iter().enumerate() {
            let _ = writeln!(out, "{}", i + 1);
            let _ = writeln!(
                out,
                "{} --> {}",
                srt_time(entry.start_ms),
                srt_time(entry.end_ms)
            );
            if both {
                let _ = writeln!(out, "[{}] {}", label(entry.source), entry.text);
            } else {
                let _ = writeln!(out, "{}", entry.text);
            }
            let _ = writeln!(out);
        }
        out
    }

    pub fn save_text(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_text())
    }

    pub fn save_srt(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_srt())
    }
}

fn label(source: Source) -> &'static str {
    match source {
        Source::System => "sistema",
        Source::Mic => "micro",
    }
}

/// `hh:mm:ss` para el volcado en texto.
fn clock(ms: u64) -> String {
    let total = ms / 1000;
    format!("{:02}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
}

/// `hh:mm:ss,mmm`, que es lo que exige SRT (coma, no punto).
fn srt_time(ms: u64) -> String {
    let total = ms / 1000;
    format!(
        "{:02}:{:02}:{:02},{:03}",
        total / 3600,
        (total / 60) % 60,
        total % 60,
        ms % 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_usa_coma_para_los_milisegundos() {
        assert_eq!(srt_time(0), "00:00:00,000");
        assert_eq!(srt_time(1_500), "00:00:01,500");
        assert_eq!(srt_time(3_661_042), "01:01:01,042");
    }

    #[test]
    fn el_parcial_sustituye_y_el_final_lo_limpia() {
        let mut t = Transcript::new();
        t.set_partial(Source::System, "hola");
        t.set_partial(Source::System, "hola que tal");
        assert_eq!(t.pending(Source::System), Some("hola que tal"));
        let entry = t
            .push_final(Source::System, " hola que tal ", 1_000, 4_000)
            .expect("hay linea");
        assert_eq!(entry.text, "hola que tal");
        assert_eq!((entry.start_ms, entry.end_ms), (1_000, 4_000));
        assert_eq!(t.pending(Source::System), None);
        assert_eq!(t.entries().len(), 1);
    }

    #[test]
    fn un_final_vacio_no_genera_linea() {
        let mut t = Transcript::new();
        t.set_partial(Source::Mic, "eh");
        assert!(t.push_final(Source::Mic, "   ", 0, 100).is_none());
        assert!(t.entries().is_empty());
        assert_eq!(t.pending(Source::Mic), None);
    }

    #[test]
    fn las_fuentes_no_se_mezclan() {
        let mut t = Transcript::new();
        t.set_partial(Source::System, "de la peli");
        t.set_partial(Source::Mic, "lo que digo yo");
        assert_eq!(t.pending(Source::System), Some("de la peli"));
        assert_eq!(t.pending(Source::Mic), Some("lo que digo yo"));
        t.push_final(Source::Mic, "lo que digo yo", 0, 1_000);
        assert_eq!(t.pending(Source::System), Some("de la peli"));
    }

    #[test]
    fn el_txt_va_ordenado_por_inicio_con_la_fuente() {
        let mut t = Transcript::new();
        t.push_final(Source::Mic, "segunda", 65_000, 66_000);
        t.push_final(Source::System, "primera", 3_661_000, 3_662_000);
        t.push_final(Source::System, "cero", 1_000, 2_000);
        assert_eq!(
            t.to_text(),
            "[00:00:01] [sistema] cero\n[00:01:05] [micro] segunda\n[01:01:01] [sistema] primera\n"
        );
    }

    #[test]
    fn el_srt_va_numerado_desde_uno_sin_prefijo_con_una_sola_fuente() {
        let mut t = Transcript::new();
        t.push_final(Source::System, "primera", 0, 1_000);
        t.push_final(Source::System, "segunda", 2_000, 3_000);
        let srt = t.to_srt();
        assert!(srt.starts_with("1\n00:00:00,000 --> 00:00:01,000\nprimera\n"));
        assert!(srt.contains("2\n00:00:02,000 --> 00:00:03,000\nsegunda\n"));
        assert!(!srt.contains('['));
    }

    #[test]
    fn el_srt_con_dos_fuentes_lleva_prefijo_y_orden_por_inicio() {
        let mut t = Transcript::new();
        t.push_final(Source::Mic, "yo", 2_500, 4_250);
        t.push_final(Source::System, "ellos", 500, 2_000);
        assert_eq!(
            t.to_srt(),
            "1\n00:00:00,500 --> 00:00:02,000\n[sistema] ellos\n\n\
             2\n00:00:02,500 --> 00:00:04,250\n[micro] yo\n\n"
        );
    }

    #[test]
    fn el_fin_nunca_queda_antes_del_inicio() {
        let mut t = Transcript::new();
        let entry = t.push_final(Source::Mic, "x", 5_000, 100).unwrap();
        assert_eq!(entry.end_ms, 5_000);
    }

    #[test]
    fn clear_deja_todo_vacio() {
        let mut t = Transcript::new();
        t.set_partial(Source::Mic, "a");
        t.push_final(Source::Mic, "a", 0, 1);
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.pending(Source::Mic), None);
    }
}
