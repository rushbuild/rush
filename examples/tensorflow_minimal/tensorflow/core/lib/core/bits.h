#ifndef TENSORFLOW_CORE_LIB_CORE_BITS_H_
#define TENSORFLOW_CORE_LIB_CORE_BITS_H_

namespace tensorflow {
namespace core {

inline int CountLeadingZeros(unsigned int x) {
  return __builtin_clz(x);
}

}  // namespace core
}  // namespace tensorflow

#endif  // TENSORFLOW_CORE_LIB_CORE_BITS_H_
