#ifndef TENSORFLOW_CORE_LIB_CORE_ARENA_H_
#define TENSORFLOW_CORE_LIB_CORE_ARENA_H_

#include <cstddef>

namespace tensorflow {
namespace core {

class Arena {
 public:
  Arena() {}
  void* Allocate(std::size_t bytes);
};

}  // namespace core
}  // namespace tensorflow

#endif  // TENSORFLOW_CORE_LIB_CORE_ARENA_H_
