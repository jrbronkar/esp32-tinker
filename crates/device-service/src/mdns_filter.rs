//! Add question type/class matching while leaving DNS parsing/encoding to edge-mdns.
use edge_mdns::{
    domain::base::{
        iana::{Class, Rtype},
        Message, ToName,
    },
    HostAnswer, HostAnswers, HostAnswersMdnsHandler, MdnsError, MdnsHandler, MdnsRequest,
    MdnsResponse,
};

// The top class bit is the mDNS unicast-response flag in questions.
const CLASS_VALUE_MASK: u16 = 0x7fff;

pub fn respond<'a>(
    answers: &impl HostAnswers,
    request: MdnsRequest<'_>,
    buffer: &'a mut [u8],
) -> Result<MdnsResponse<'a>, MdnsError> {
    if let MdnsRequest::Request { data, .. } = &request {
        let filtered = QuestionAnswers {
            answers,
            query: Message::from_octets(*data)?,
        };
        HostAnswersMdnsHandler::new(filtered).handle(request, buffer)
    } else {
        HostAnswersMdnsHandler::new(answers).handle(request, buffer)
    }
}

struct QuestionAnswers<'a, A> {
    answers: &'a A,
    query: Message<&'a [u8]>,
}
impl<A: HostAnswers> HostAnswers for QuestionAnswers<'_, A> {
    fn visit<F, E>(&self, mut f: F) -> Result<(), E>
    where
        F: FnMut(HostAnswer) -> Result<(), E>,
        E: From<MdnsError>,
    {
        self.answers.visit(|answer| {
            let mut queried_name = false;
            for question in self.query.question() {
                let question = question.map_err(MdnsError::from)?;
                if question.qname().name_eq(&answer.owner()) {
                    queried_name = true;
                    let class = question.qclass().to_int() & CLASS_VALUE_MASK;
                    let record_class = answer.class().to_int() & CLASS_VALUE_MASK;
                    if (class == record_class || class == Class::ANY.to_int())
                        && (question.qtype() == answer.rtype() || question.qtype() == Rtype::ANY)
                    {
                        return f(answer);
                    }
                }
            }
            // Keep records with other owners available for the additional section,
            // e.g. the A/SRV/TXT records that accompany a matching PTR answer.
            if !queried_name {
                f(answer)
            } else {
                Ok(())
            }
        })
    }
}
