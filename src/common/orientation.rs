//! Axis orientation vocabulary translated from Bio-Formats' orientation API.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrientationType {
    Anatomical,
}

impl OrientationType {
    pub fn defined_type(self) -> &'static str {
        "anatomical"
    }
    pub fn from_string(value: &str) -> Option<Self> {
        (value == "anatomical").then_some(Self::Anatomical)
    }
}

pub trait OrientationTerm: Copy {
    fn defined_term(self) -> &'static str;
    fn flip(self) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnatomicalOrientation {
    LeftToRight,
    RightToLeft,
    AnteriorToPosterior,
    PosteriorToAnterior,
    InferiorToSuperior,
    SuperiorToInferior,
    DorsalToVentral,
    VentralToDorsal,
    DorsalToPalmar,
    PalmarToDorsal,
    DorsalToPlantar,
    PlantarToDorsal,
    RostralToCaudal,
    CaudalToRostral,
    CranialToCaudal,
    CaudalToCranial,
    ProximalToDistal,
    DistalToProximal,
    SuperficialToDeep,
    DeepToSuperficial,
    ApicalToBasal,
    BasalToApical,
    ApexToBase,
    BaseToApex,
}

impl AnatomicalOrientation {
    pub fn from_string(value: &str) -> Option<Self> {
        match value {
            "left-to-right" => Some(Self::LeftToRight),
            "right-to-left" => Some(Self::RightToLeft),
            "anterior-to-posterior" => Some(Self::AnteriorToPosterior),
            "posterior-to-anterior" => Some(Self::PosteriorToAnterior),
            "inferior-to-superior" => Some(Self::InferiorToSuperior),
            "superior-to-inferior" => Some(Self::SuperiorToInferior),
            "dorsal-to-ventral" => Some(Self::DorsalToVentral),
            "ventral-to-dorsal" => Some(Self::VentralToDorsal),
            "dorsal-to-palmar" => Some(Self::DorsalToPalmar),
            "palmar-to-dorsal" => Some(Self::PalmarToDorsal),
            "dorsal-to-plantar" => Some(Self::DorsalToPlantar),
            "plantar-to-dorsal" => Some(Self::PlantarToDorsal),
            "rostral-to-caudal" => Some(Self::RostralToCaudal),
            "caudal-to-rostral" => Some(Self::CaudalToRostral),
            "cranial-to-caudal" => Some(Self::CranialToCaudal),
            "caudal-to-cranial" => Some(Self::CaudalToCranial),
            "proximal-to-distal" => Some(Self::ProximalToDistal),
            "distal-to-proximal" => Some(Self::DistalToProximal),
            "superficial-to-deep" => Some(Self::SuperficialToDeep),
            "deep-to-superficial" => Some(Self::DeepToSuperficial),
            "apical-to-basal" => Some(Self::ApicalToBasal),
            "basal-to-apical" => Some(Self::BasalToApical),
            "apex-to-base" => Some(Self::ApexToBase),
            "base-to-apex" => Some(Self::BaseToApex),
            _ => None,
        }
    }
}

impl OrientationTerm for AnatomicalOrientation {
    fn defined_term(self) -> &'static str {
        match self {
            Self::LeftToRight => "left-to-right",
            Self::RightToLeft => "right-to-left",
            Self::AnteriorToPosterior => "anterior-to-posterior",
            Self::PosteriorToAnterior => "posterior-to-anterior",
            Self::InferiorToSuperior => "inferior-to-superior",
            Self::SuperiorToInferior => "superior-to-inferior",
            Self::DorsalToVentral => "dorsal-to-ventral",
            Self::VentralToDorsal => "ventral-to-dorsal",
            Self::DorsalToPalmar => "dorsal-to-palmar",
            Self::PalmarToDorsal => "palmar-to-dorsal",
            Self::DorsalToPlantar => "dorsal-to-plantar",
            Self::PlantarToDorsal => "plantar-to-dorsal",
            Self::RostralToCaudal => "rostral-to-caudal",
            Self::CaudalToRostral => "caudal-to-rostral",
            Self::CranialToCaudal => "cranial-to-caudal",
            Self::CaudalToCranial => "caudal-to-cranial",
            Self::ProximalToDistal => "proximal-to-distal",
            Self::DistalToProximal => "distal-to-proximal",
            Self::SuperficialToDeep => "superficial-to-deep",
            Self::DeepToSuperficial => "deep-to-superficial",
            Self::ApicalToBasal => "apical-to-basal",
            Self::BasalToApical => "basal-to-apical",
            Self::ApexToBase => "apex-to-base",
            Self::BaseToApex => "base-to-apex",
        }
    }
    fn flip(self) -> Self {
        match self {
            Self::LeftToRight => Self::RightToLeft,
            Self::RightToLeft => Self::LeftToRight,
            Self::AnteriorToPosterior => Self::PosteriorToAnterior,
            Self::PosteriorToAnterior => Self::AnteriorToPosterior,
            Self::InferiorToSuperior => Self::SuperiorToInferior,
            Self::SuperiorToInferior => Self::InferiorToSuperior,
            Self::DorsalToVentral => Self::VentralToDorsal,
            Self::VentralToDorsal => Self::DorsalToVentral,
            Self::DorsalToPalmar => Self::PalmarToDorsal,
            Self::PalmarToDorsal => Self::DorsalToPalmar,
            Self::DorsalToPlantar => Self::PlantarToDorsal,
            Self::PlantarToDorsal => Self::DorsalToPlantar,
            Self::RostralToCaudal => Self::CaudalToRostral,
            Self::CaudalToRostral => Self::RostralToCaudal,
            Self::CranialToCaudal => Self::CaudalToCranial,
            Self::CaudalToCranial => Self::CranialToCaudal,
            Self::ProximalToDistal => Self::DistalToProximal,
            Self::DistalToProximal => Self::ProximalToDistal,
            Self::SuperficialToDeep => Self::DeepToSuperficial,
            Self::DeepToSuperficial => Self::SuperficialToDeep,
            Self::ApicalToBasal => Self::BasalToApical,
            Self::BasalToApical => Self::ApicalToBasal,
            Self::ApexToBase => Self::BaseToApex,
            Self::BaseToApex => Self::ApexToBase,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Orientation {
    pub orientation_type: OrientationType,
    pub term: AnatomicalOrientation,
}

impl Orientation {
    pub fn new(orientation_type: OrientationType, term: AnatomicalOrientation) -> Self {
        Self {
            orientation_type,
            term,
        }
    }
    pub fn flip(self) -> Self {
        Self {
            term: self.term.flip(),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn orientation_vocabulary_roundtrips_and_flips() {
        let term = AnatomicalOrientation::LeftToRight;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::RightToLeft;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::AnteriorToPosterior;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::PosteriorToAnterior;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::InferiorToSuperior;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::SuperiorToInferior;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::DorsalToVentral;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::VentralToDorsal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::DorsalToPalmar;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::PalmarToDorsal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::DorsalToPlantar;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::PlantarToDorsal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::RostralToCaudal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::CaudalToRostral;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::CranialToCaudal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::CaudalToCranial;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::ProximalToDistal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::DistalToProximal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::SuperficialToDeep;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::DeepToSuperficial;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::ApicalToBasal;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::BasalToApical;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::ApexToBase;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
        let term = AnatomicalOrientation::BaseToApex;
        assert_eq!(
            AnatomicalOrientation::from_string(term.defined_term()),
            Some(term)
        );
        assert_eq!(term.flip().flip(), term);
        assert_ne!(term.flip(), term);
    }
}
